//! Pure causal structural replay over the existing seat history.
//!
//! The result is a temporary checked projection, not a writable registry or
//! authority. Consumers must separately revalidate current membership before
//! dispatch. No Core construction, global allocation, event append or effects.
//! A local completion must equal its recomputed deterministic delta; a decoded
//! request alone never publishes a layout or consumes an allocation interval.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::Deserialize;

use super::admission_operation::UiPreparedPresentationOperation;
use super::binding::{
    UiBindingAttachment, UiBindingAttachmentCoordinate, UiPresentationMembership,
    UiSurfaceMembership,
};
use super::presentation_initialization::UiPresentationInitialization;
use super::presentation_state::PresentationComposition;
use super::seat::{KEY_ACTIVE_SURFACE, SeatEvent, SeatEventKind, UiSurfaceSelection};
use super::transfer_reservation::UiTransferReservation;
use crate::ids::{RyeOsViewInstanceKey, ViewSetId};

/// Derived subject metadata is tied to the exact mounted payload. It does not
/// replace explicit selection facets in the seat history (including null).
#[derive(Clone, Debug, PartialEq)]
pub struct UiCompositionMountSubject {
    pub binding: UiBindingAttachmentCoordinate,
    pub view_ref: String,
    pub required_input: Option<String>,
    pub attachment: super::attachment::SelectionAttachment,
}

/// Transient output derived from one ordered history. Never serialize this as
/// an alternative replay base. Incomplete operations remain retained intent.
#[derive(Clone, Debug)]
pub struct UiCompositionReplay {
    pub composition: PresentationComposition,
    pub revision: u64,
    pub next_mount_id: u64,
    pub pending: BTreeMap<String, UiPreparedPresentationOperation>,
    /// Exact retained obligations; completion acknowledges but never releases
    /// a freeze. Release requires the separately authenticated transfer outcome.
    pub move_freezes: BTreeMap<String, super::transfer_freeze::UiMoveFreezeRequest>,
    move_freeze_positions: BTreeMap<String, (u64, Option<u64>)>,
    /// Committed source obligations are historical witnesses, not active
    /// mutation fences after their exact inventory has been retired.
    committed_move_freezes: BTreeMap<
        String,
        (
            super::transfer_freeze::UiMoveFreezeRequest,
            (u64, Option<u64>),
        ),
    >,
    // Historical duplicate-ID index. Cloning a checked projection for a
    // read must not copy every previously settled operation. The owned
    // acknowledged fold normally has exclusive custody before insertion.
    pub settled: Arc<BTreeSet<String>>,
    pub subjects: BTreeMap<RyeOsViewInstanceKey, UiCompositionMountSubject>,
    /// Exact local-key moves produced only by settled composition completions.
    /// The operation ID lets the live ACK owner apply a move once; historical
    /// replay never makes an old move into a new local instruction.
    pub(crate) completed_mount_remaps:
        BTreeMap<String, BTreeMap<RyeOsViewInstanceKey, RyeOsViewInstanceKey>>,
    pub lens_frames:
        BTreeMap<RyeOsViewInstanceKey, Vec<super::composition_lens::RetainedLensFrame>>,
    pub input_routes: BTreeMap<RyeOsViewInstanceKey, (u64, Option<serde_json::Value>)>,
    /// Mapped signed MOVE drafts only. Core hydration remains a separate
    /// current-session operation and never derives admission from this map.
    pub transfer_drafts: BTreeMap<super::model::InputBufferKey, super::model::RyeOsInputState>,
    transfer_draft_keys: Arc<BTreeSet<super::model::InputBufferKey>>,
    pub lens_labels: BTreeMap<RyeOsViewInstanceKey, Option<String>>,
    pub work_selections: BTreeMap<ViewSetId, serde_json::Value>,
    pub insertion_bindings: BTreeMap<ViewSetId, UiBindingAttachmentCoordinate>,
    pub template_ids: BTreeMap<ViewSetId, String>,
    /// Original admitted metadata for live Surfaces, never a mutable-layout claim.
    pub admitted_surfaces: BTreeMap<String, UiSurfaceMembership>,
    /// Exact authenticated closure testimony for transient membership repair.
    pub closed_surfaces: Arc<BTreeMap<String, super::seat::UiSurfaceClosure>>,
    initialization: UiPresentationInitialization,
    last_chain_seq: Option<u64>,
    used_surfaces: Arc<BTreeSet<String>>,
    used_ordinals: Arc<BTreeSet<u64>>,
    descriptors: Arc<BTreeMap<String, UiBindingAttachment>>,
    /// Hidden accepted allocations consume their IDs even if later released.
    /// Their descriptors are not placed in the live descriptor map until a
    /// separately authenticated activation installs the exact mount.
    accepted_transfers: BTreeMap<String, UiTransferReservation>,
    used_transfer_binding_ids: Arc<BTreeSet<String>>,
    paired_transfer_seen: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AdmissionTransition {
    kind: String,
    presentation_id: String,
    surface: UiSurfaceMembership,
    attachment: UiBindingAttachment,
    creation_request_id: String,
    #[serde(default)]
    opening: Option<crate::surface::opening::SurfaceOpenRequest>,
    #[serde(default)]
    tab_attachments: Option<Vec<UiBindingAttachment>>,
}

fn coordinate(value: &UiBindingAttachment) -> UiBindingAttachmentCoordinate {
    UiBindingAttachmentCoordinate {
        binding_attachment_id: value.binding_attachment_id.clone(),
        binding_generation: value.binding_generation,
        binding_digest: value.binding_digest.clone(),
    }
}

fn validate_transfer_subject(
    attachment: &super::attachment::SelectionAttachment,
    descriptor: &UiBindingAttachment,
    view_ref: &str,
    instance: &RyeOsViewInstanceKey,
    composition: &PresentationComposition,
    max_bytes: u64,
) -> Result<(), String> {
    let bound = max_bytes.min(descriptor.binding_request_bounds.max_request_bytes);
    if bound == 0
        || serde_json::to_vec(attachment)
            .map_err(|error| error.to_string())?
            .len() as u64
            > bound
    {
        return Err("transfer subject exceeds exact destination binding bound".into());
    }
    let raw = descriptor
        .presentation
        .views
        .get(view_ref)
        .ok_or("transfer subject View is outside destination closure")?;
    let view: super::content::ViewBinding =
        serde_json::from_value(raw.clone()).map_err(|error| error.to_string())?;
    let dependencies = super::attachment::selection_dependencies(&view);
    match attachment {
        super::attachment::SelectionAttachment::FollowViewSet { view_set_id } => {
            let Some(crate::ids::UiViewMountOwner::Surface {
                surface_instance_id,
                ..
            }) = instance.mount_owner()
            else {
                return Err("shared transfer View cannot follow a Surface selection".into());
            };
            composition
                .view_set(&surface_instance_id, *view_set_id)
                .ok_or("transfer follower target is not in destination Surface")?;
        }
        super::attachment::SelectionAttachment::Pinned {
            values,
            fingerprint,
        } => {
            use sha2::{Digest, Sha256};
            if values.keys().cloned().collect::<BTreeSet<_>>() != dependencies
                || values.values().any(serde_json::Value::is_null)
                || *fingerprint
                    != format!(
                        "{:x}",
                        Sha256::digest(
                            serde_json::to_vec(values).map_err(|error| error.to_string())?
                        )
                    )
            {
                return Err("transfer pin differs from destination View dependencies".into());
            }
        }
        super::attachment::SelectionAttachment::RequiredSubject { input, facets } => {
            if input.trim().is_empty()
                || input.trim() != input
                || input.chars().any(char::is_control)
                || facets.is_empty()
                || facets.len() != dependencies.len()
                || facets.iter().cloned().collect::<BTreeSet<_>>() != dependencies
            {
                return Err(
                    "transfer unresolved subject differs from destination View dependencies".into(),
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn mapped_transfer_draft_binding<'a>(
    key: &super::model::InputBufferKey,
    candidates: impl IntoIterator<
        Item = (
            &'a str,
            &'a UiBindingAttachmentCoordinate,
            &'a UiBindingAttachmentCoordinate,
        ),
    >,
) -> Result<&'a UiBindingAttachmentCoordinate, String> {
    let mut mapped = None;
    for (view_ref, source, destination) in candidates {
        if key.view_ref == view_ref
            && key.binding_scope == super::model::InputBufferKey::binding_scope(source)
        {
            if mapped.is_some_and(|prior| prior != destination) {
                return Err("activated draft has ambiguous frame binding".into());
            }
            mapped = Some(destination);
        }
    }
    mapped.ok_or("activated draft has no exact source scope".into())
}

impl UiCompositionReplay {
    /// Compare the original replay testimony, not the subsequently edited
    /// composition. Authentication of that testimony belongs to the caller.
    pub fn matches_initialization(&self, initial: &UiPresentationInitialization) -> bool {
        &self.initialization == initial
    }

    fn initialized(initial: &UiPresentationInitialization) -> Result<Self, String> {
        initial.validate()?;
        let root = initial
            .binding_attachments
            .iter()
            .find(|value| coordinate(value) == initial.root_binding)
            .ok_or("initialization has no exact root descriptor")?;
        let membership = UiPresentationMembership {
            presentation_id: initial.presentation_id.clone(),
            root_binding: initial.root_binding.clone(),
            thread_id: None,
            chain_root_id: None,
            next_mount_id: initial.next_mount_id,
            initialization_digest: initial.digest()?,
            surfaces: initial.surfaces.clone(),
        };
        let mut composition = PresentationComposition::from_membership(&membership, root)?;
        let descriptors = initial
            .binding_attachments
            .iter()
            .map(|descriptor| (descriptor.binding_attachment_id.clone(), descriptor.clone()))
            .collect::<BTreeMap<_, _>>();
        for surface in &initial.surfaces {
            let descriptor = initial
                .binding_attachments
                .iter()
                .find(|value| coordinate(value) == surface.binding)
                .ok_or("initialization has no exact Surface descriptor")?;
            if surface.saved_surface.is_some() {
                composition.open_saved_surface(surface, descriptor, &descriptors)?;
            } else {
                composition.open_surface(surface, descriptor)?;
            }
        }
        let mut replay = Self {
            composition,
            revision: 0,
            next_mount_id: initial.next_mount_id,
            pending: BTreeMap::new(),
            settled: Arc::new(BTreeSet::new()),
            subjects: BTreeMap::new(),
            completed_mount_remaps: BTreeMap::new(),
            lens_frames: BTreeMap::new(),
            input_routes: BTreeMap::new(),
            transfer_drafts: BTreeMap::new(),
            transfer_draft_keys: Arc::new(BTreeSet::new()),
            lens_labels: BTreeMap::new(),
            work_selections: BTreeMap::new(),
            insertion_bindings: BTreeMap::new(),
            template_ids: initial
                .surfaces
                .iter()
                .flat_map(|surface| {
                    surface
                        .mount_manifest
                        .tabs
                        .iter()
                        .map(|tab| (tab.view_set_id, tab.seed.id.clone()))
                })
                .collect(),
            admitted_surfaces: initial
                .surfaces
                .iter()
                .map(|surface| (surface.surface_instance_id.clone(), surface.clone()))
                .collect(),
            closed_surfaces: Arc::new(BTreeMap::new()),
            initialization: initial.clone(),
            last_chain_seq: None,
            move_freezes: BTreeMap::new(),
            move_freeze_positions: BTreeMap::new(),
            committed_move_freezes: BTreeMap::new(),
            used_surfaces: Arc::new(
                initial
                    .surfaces
                    .iter()
                    .map(|surface| surface.surface_instance_id.clone())
                    .collect(),
            ),
            used_ordinals: Arc::new(
                initial
                    .surfaces
                    .iter()
                    .map(|surface| surface.ordinal)
                    .collect(),
            ),
            descriptors: Arc::new(descriptors),
            accepted_transfers: BTreeMap::new(),
            used_transfer_binding_ids: Arc::new(BTreeSet::new()),
            paired_transfer_seen: false,
        };
        replay.reconcile_subject_mounts()?;
        for surface in &initial.surfaces {
            if surface.saved_surface.is_some() {
                replay
                    .hydrate_admitted_saved_subjects(&surface.mount_manifest.surface_instance_id)?;
            }
        }
        Ok(replay)
    }

    /// All rows must carry authoritative chain sequence, in original order.
    /// Foreign daemon events are filtered by the existing typed seat decoder;
    /// malformed owned events must not be dropped before calling this function.
    /// A failure returns no partial projection to the caller.
    pub fn from_history(
        initial: &UiPresentationInitialization,
        events: &[SeatEvent],
    ) -> Result<Self, String> {
        let first = events
            .first()
            .ok_or("structural replay requires initialization testimony")?;
        let mut replay = Self::begin(initial, first)?;
        for event in &events[1..] {
            replay = replay.apply_ordered_event_owned(event)?;
        }
        Ok(replay)
    }
    /// Fold an already acknowledged client prefix. Its sequence is the local
    /// monotonic causal order, not a claimed daemon chain coordinate. Adapters
    /// must not pass an unacknowledged suffix through this publication boundary.
    pub fn from_acknowledged_prefix(
        initial: &UiPresentationInitialization,
        events: &[SeatEvent],
    ) -> Result<Self, String> {
        Self::from_history(initial, events)
    }

    pub fn binding_descriptors(&self) -> &BTreeMap<String, UiBindingAttachment> {
        &self.descriptors
    }

    /// A one-seat event log cannot rebuild this derivative after a historical
    /// replacement: paired rows intentionally do not become SeatEvents.
    pub(crate) fn requires_paired_reconstruction(&self) -> bool {
        self.paired_transfer_seen
    }

    /// Stage one destination Accepted from a separately completed paired cut.
    /// No mount, descriptor, revision, session grant, or browser completion is
    /// published.
    /// The caller must supply this lifecycle from the completed paired proof;
    /// a decoded lifecycle alone is not proof of a signed seat cut.
    pub fn apply_authenticated_transfer_acceptance(
        &mut self,
        lifecycle: &ryeos_ui_presentation_contract::transfer_state::TransferLifecycleState,
        max_bytes: u64,
    ) -> Result<(), String> {
        let prepared = lifecycle
            .prepared()
            .ok_or("transfer acceptance has no completed source preparation")?;
        let accepted = lifecycle
            .accepted()
            .ok_or("transfer acceptance has no completed destination acceptance")?;
        let transfer = lifecycle.transfer();
        let chain_seq = accepted.evidence.chain_seq;
        let export =
            super::transfer_export::UiTransferExport::from_preparation(prepared.record, max_bytes)?;
        let reservation = UiTransferReservation::from_acceptance(accepted.record, max_bytes)?;
        if self.composition.id().as_str() != transfer.destination.presentation_id
            || self.revision != reservation.expected_revision
            || self.next_mount_id != reservation.allocation_start
            || self
                .last_chain_seq
                .is_none_or(|previous| chain_seq <= previous)
            || self.accepted_transfers.contains_key(&transfer.operation_id)
            || self.settled.contains(&transfer.operation_id)
            || self.pending.contains_key(&transfer.operation_id)
        {
            return Err("transfer acceptance differs from destination replay frontier".into());
        }
        reservation.validate_export(transfer, &export, max_bytes)?;
        match &reservation.placement {
            super::transfer_reservation::UiTransferDestinationPlacement::View {
                surface_instance_id,
                view_set_id,
                allocation,
                placement,
            } => {
                let set = self
                    .composition
                    .view_set(surface_instance_id, *view_set_id)
                    .ok_or("transfer destination ViewSet is not mounted")?;
                super::composition_delta::validate_allocated_view_placement(
                    set, allocation, placement,
                )?;
            }
            super::transfer_reservation::UiTransferDestinationPlacement::Tabs {
                manifest, ..
            } => {
                if !self
                    .composition
                    .surfaces()
                    .contains_key(&manifest.surface_instance_id)
                    || manifest.tabs.iter().any(|tab| {
                        self.composition
                            .view_sets()
                            .any(|(_, mounted)| mounted.id == tab.view_set_id)
                    })
                {
                    return Err("transfer destination tabs collide with current composition".into());
                }
            }
            super::transfer_reservation::UiTransferDestinationPlacement::SharedDock { .. } => {}
        }
        if reservation.mounts.iter().any(|mount| {
            self.composition
                .mount(&mount.destination_instance)
                .is_some()
                || self.accepted_transfers.values().any(|pending| {
                    pending
                        .mounts
                        .iter()
                        .any(|reserved| reserved.destination_instance == mount.destination_instance)
                })
        }) {
            return Err(
                "transfer acceptance collides with a mounted or reserved destination".into(),
            );
        }
        let mut staged = self.clone();
        for descriptor in &reservation.descriptors {
            if staged
                .descriptors
                .contains_key(&descriptor.binding_attachment_id)
                || !Arc::make_mut(&mut staged.used_transfer_binding_ids)
                    .insert(descriptor.binding_attachment_id.clone())
            {
                return Err("transfer acceptance reuses a retained binding ID".into());
            }
        }
        staged.next_mount_id = reservation.next_mount_id;
        staged
            .accepted_transfers
            .insert(transfer.operation_id.clone(), reservation);
        staged.last_chain_seq = Some(chain_seq);
        *self = staged;
        Ok(())
    }

    /// Retained-data settlement and aborted reservation release publish no
    /// layout and do not restore consumed IDs or destination descriptors.
    /// The completed paired lifecycle supplies the exact terminal position.
    pub fn apply_authenticated_transfer_nonactivation(
        &mut self,
        lifecycle: &ryeos_ui_presentation_contract::transfer_state::TransferLifecycleState,
        max_bytes: u64,
    ) -> Result<(), String> {
        let terminal = match (lifecycle.retained(), lifecycle.released()) {
            (Some(terminal), None) | (None, Some(terminal)) => terminal,
            _ => return Err("transfer has no singular non-activation terminal".into()),
        };
        let accepted = lifecycle
            .accepted()
            .ok_or("terminal destination transfer has no acceptance")?;
        let reservation = UiTransferReservation::from_acceptance(accepted.record, max_bytes)?;
        if self.composition.id().as_str() != lifecycle.transfer().destination.presentation_id
            || self
                .accepted_transfers
                .get(&lifecycle.transfer().operation_id)
                != Some(&reservation)
            || self
                .last_chain_seq
                .is_none_or(|previous| terminal.evidence.chain_seq <= previous)
        {
            return Err("terminal transfer differs from hidden destination reservation".into());
        }
        self.accepted_transfers
            .remove(&lifecycle.transfer().operation_id);
        Arc::make_mut(&mut self.settled).insert(lifecycle.transfer().operation_id.clone());
        self.last_chain_seq = Some(terminal.evidence.chain_seq);
        Ok(())
    }

    /// One source MOVE Commit removes exactly the still-frozen exported
    /// inventory and advances the structural revision once. A COPY commit is
    /// deliberately not passed here. This pure paired-proof projection moves
    /// the completed freeze to historical testimony: retired source IDs no
    /// longer impose an active mutation fence, but no grant is restored.
    pub fn apply_authenticated_move_commit(
        &mut self,
        lifecycle: &ryeos_ui_presentation_contract::transfer_state::TransferLifecycleState,
        max_bytes: u64,
    ) -> Result<(), String> {
        let prepared = lifecycle
            .prepared()
            .ok_or("MOVE commit has no source preparation")?;
        let committed = lifecycle
            .committed()
            .ok_or("MOVE source has no authenticated commit")?;
        let transfer = lifecycle.transfer();
        let export =
            super::transfer_export::UiTransferExport::from_preparation(prepared.record, max_bytes)?;
        if export.mode != super::transfer_export::UiTransferExportMode::Move
            || self.composition.id().as_str() != transfer.source.presentation_id
            || self
                .last_chain_seq
                .is_none_or(|previous| committed.evidence.chain_seq <= previous)
            || !self.move_freezes.iter().any(|(freeze_id, freeze)| {
                freeze.transfer == *transfer
                    && freeze.export == export
                    && self.settled.contains(freeze_id)
            })
        {
            return Err("MOVE commit differs from settled frozen source".into());
        }
        export.validate_affected_snapshot(transfer, self, max_bytes)?;
        let mut staged = self.clone();
        match &export.content {
            super::transfer_export::UiTransferExportContent::ViewSets { sets, .. } => {
                for set in sets {
                    let crate::ids::UiViewMountOwner::Surface {
                        surface_instance_id,
                        ..
                    } = &set.source_owner
                    else {
                        return Err("MOVE set has no Surface owner".into());
                    };
                    staged
                        .composition
                        .close_view_set(surface_instance_id, set.source_view_set_id)?;
                }
            }
            super::transfer_export::UiTransferExportContent::SurfaceMount {
                source_view_set_id,
                mount,
            } => {
                let Some(crate::ids::UiViewMountOwner::Surface {
                    surface_instance_id,
                    ..
                }) = mount.source_instance.mount_owner()
                else {
                    return Err("MOVE mount has no Surface owner".into());
                };
                let tile_id = mount
                    .source_instance
                    .surface_tile_id()
                    .ok_or("MOVE mount has no tile identity")?;
                staged.composition.mutate_view_set(
                    &surface_instance_id,
                    *source_view_set_id,
                    BTreeMap::new(),
                    |set| {
                        if !set.close_tile(tile_id) {
                            return Err("MOVE source tile is absent".into());
                        }
                        Ok(())
                    },
                )?;
            }
            super::transfer_export::UiTransferExportContent::SharedDock { edge, mount, .. } => {
                let edge = match edge {
                    super::model::RyeOsDockEdge::Top => "top",
                    super::model::RyeOsDockEdge::Left => "left",
                    super::model::RyeOsDockEdge::Right => "right",
                    super::model::RyeOsDockEdge::Bottom => "bottom",
                };
                let removed = staged
                    .composition
                    .remove_shared_dock(edge)
                    .ok_or("MOVE shared dock is absent")?;
                if removed.view().instance_key() != &mount.source_instance {
                    return Err("MOVE shared dock changed its instance".into());
                }
            }
        }
        staged.reconcile_subject_mounts()?;
        let exported = export
            .mounts()
            .into_iter()
            .map(|mount| mount.source_instance.clone())
            .collect::<BTreeSet<_>>();
        staged
            .transfer_drafts
            .retain(|key, _| !exported.contains(&key.view_instance_key));
        let freeze_id = staged
            .move_freezes
            .iter()
            .find(|(id, freeze)| {
                freeze.transfer == *transfer
                    && freeze.export == export
                    && staged.settled.contains(*id)
            })
            .map(|(id, _)| id.clone())
            .ok_or("MOVE commit lost its settled freeze")?;
        let freeze = staged
            .move_freezes
            .remove(&freeze_id)
            .ok_or("MOVE commit lost its active freeze")?;
        let positions = staged
            .move_freeze_positions
            .remove(&freeze_id)
            .filter(|(_, completion)| completion.is_some())
            .ok_or("MOVE commit freeze has no settled completion positions")?;
        staged
            .committed_move_freezes
            .insert(freeze_id, (freeze, positions));
        staged.advance_revision()?;
        staged.last_chain_seq = Some(committed.evidence.chain_seq);
        *self = staged;
        Ok(())
    }

    /// Source Abort settles the retained MOVE freeze without changing layout
    /// or revision. It does not authorize a client-side freeze release by
    /// itself; only the paired caller may use this projection.
    pub fn apply_authenticated_move_abort(
        &mut self,
        lifecycle: &ryeos_ui_presentation_contract::transfer_state::TransferLifecycleState,
        max_bytes: u64,
    ) -> Result<(), String> {
        let prepared = lifecycle
            .prepared()
            .ok_or("MOVE abort has no source preparation")?;
        let aborted = lifecycle
            .aborted()
            .ok_or("MOVE source has no authenticated abort")?;
        let transfer = lifecycle.transfer();
        let export =
            super::transfer_export::UiTransferExport::from_preparation(prepared.record, max_bytes)?;
        let freeze_id = self
            .move_freezes
            .iter()
            .find(|(id, freeze)| {
                freeze.transfer == *transfer
                    && freeze.export == export
                    && self.settled.contains(*id)
            })
            .map(|(id, _)| id.clone())
            .ok_or("MOVE abort has no exact settled freeze")?;
        if export.mode != super::transfer_export::UiTransferExportMode::Move
            || self.composition.id().as_str() != transfer.source.presentation_id
            || self
                .last_chain_seq
                .is_none_or(|previous| aborted.evidence.chain_seq <= previous)
        {
            return Err("MOVE abort differs from source replay frontier".into());
        }
        self.move_freezes.remove(&freeze_id);
        self.move_freeze_positions.remove(&freeze_id);
        self.last_chain_seq = Some(aborted.evidence.chain_seq);
        Ok(())
    }

    /// The source's terminal observation settles the historical committed
    /// MOVE witness. Its active mutation fence ended when Commit retired the
    /// exact inventory; observation adds no revision.
    pub fn apply_authenticated_move_settlement_observed(
        &mut self,
        lifecycle: &ryeos_ui_presentation_contract::transfer_state::TransferLifecycleState,
        max_bytes: u64,
    ) -> Result<(), String> {
        let prepared = lifecycle
            .prepared()
            .ok_or("MOVE observation has no preparation")?;
        let observed = lifecycle
            .settlement_observed()
            .ok_or("MOVE source has no terminal observation")?;
        let transfer = lifecycle.transfer();
        let export =
            super::transfer_export::UiTransferExport::from_preparation(prepared.record, max_bytes)?;
        if export.mode != super::transfer_export::UiTransferExportMode::Move
            || self.composition.id().as_str() != transfer.source.presentation_id
            || self
                .last_chain_seq
                .is_none_or(|previous| observed.evidence.chain_seq <= previous)
        {
            return Err("MOVE observation differs from source replay frontier".into());
        }
        let freeze_id = self
            .committed_move_freezes
            .iter()
            .find(|(_, (freeze, _))| freeze.transfer == *transfer && freeze.export == export)
            .map(|(id, _)| id.clone());
        if lifecycle.committed().is_some() {
            let freeze_id = freeze_id.ok_or("committed MOVE observation has no retained freeze")?;
            self.committed_move_freezes.remove(&freeze_id);
        } else if lifecycle.aborted().is_some() {
            if freeze_id.is_some()
                || self
                    .move_freezes
                    .values()
                    .any(|freeze| freeze.transfer == *transfer)
            {
                return Err("aborted MOVE observation retained a released freeze".into());
            }
        } else {
            return Err("MOVE observation has no source decision".into());
        }
        Arc::make_mut(&mut self.settled).insert(transfer.operation_id.clone());
        self.last_chain_seq = Some(observed.evidence.chain_seq);
        Ok(())
    }

    /// Materialize one destination Activated from the exact completed paired
    /// lifecycle. All destinations, descriptors and local values come from
    /// the signed Prepared/Accepted bodies; no private session grant is copied.
    pub fn apply_authenticated_transfer_activation(
        &mut self,
        lifecycle: &ryeos_ui_presentation_contract::transfer_state::TransferLifecycleState,
        max_bytes: u64,
    ) -> Result<(), String> {
        use super::transfer_export::{UiTransferExportContent, UiTransferExportMode};
        use super::transfer_reservation::UiTransferDestinationPlacement;
        let prepared = lifecycle
            .prepared()
            .ok_or("activation has no preparation")?;
        let accepted = lifecycle.accepted().ok_or("activation has no acceptance")?;
        let activated = lifecycle
            .activated()
            .ok_or("activation has no destination outcome")?;
        let transfer = lifecycle.transfer();
        let export =
            super::transfer_export::UiTransferExport::from_preparation(prepared.record, max_bytes)?;
        let reservation = UiTransferReservation::from_acceptance(accepted.record, max_bytes)?;
        reservation.validate_export(transfer, &export, max_bytes)?;
        if self.composition.id().as_str() != transfer.destination.presentation_id
            || self.revision < reservation.expected_revision
            || self.next_mount_id < reservation.next_mount_id
            || self.accepted_transfers.get(&transfer.operation_id) != Some(&reservation)
            || self
                .last_chain_seq
                .is_none_or(|previous| activated.evidence.chain_seq <= previous)
        {
            return Err("activation differs from hidden exact reservation frontier".into());
        }
        let mut staged = self.clone();
        for descriptor in &reservation.descriptors {
            staged.retain_descriptor(descriptor.clone())?;
        }
        let source_mounts = export.mounts();
        match (&reservation.placement, &export.content) {
            (
                UiTransferDestinationPlacement::Tabs {
                    manifest,
                    sets,
                    active_view_set_id,
                },
                UiTransferExportContent::ViewSets { .. },
            ) => {
                let surface = staged
                    .composition
                    .surfaces()
                    .get(&manifest.surface_instance_id)
                    .ok_or("activation destination Surface is absent")?;
                let surface_descriptor = staged
                    .descriptors
                    .get(&surface.binding().binding_attachment_id)
                    .filter(|descriptor| coordinate(descriptor) == *surface.binding())
                    .ok_or("activation destination Surface descriptor differs")?;
                let surface_value = surface_descriptor
                    .presentation
                    .surface_value()
                    .map_err(|error| error.to_string())?
                    .ok_or("activation destination descriptor is not a Surface")?;
                let surface_spec: crate::surface::SurfaceSpec =
                    serde_json::from_value(surface_value).map_err(|error| {
                        format!("activation Surface definition is invalid: {error}")
                    })?;
                let mut installed = manifest.instantiate_with_exact_ids(&surface_spec.tiling)?;
                for (set, allocated_set) in installed.iter_mut().zip(sets) {
                    if set.id != allocated_set.destination_view_set_id {
                        return Err("activation set differs from exact allocation".into());
                    }
                    if set.root.is_some() && allocated_set.focused_tile_id.is_none() {
                        return Err("activated nonempty tab has no exact focus".into());
                    }
                    set.focused_tile = allocated_set
                        .focused_tile_id
                        .unwrap_or(crate::ids::TileId::new(0));
                    for tile in set.tiles.values_mut() {
                        let index = reservation
                            .mounts
                            .iter()
                            .position(|mount| mount.destination_instance == tile.instance_key)
                            .ok_or("activation tile has no reserved mount")?;
                        let source = source_mounts[index];
                        if tile.view.view_ref != source.view_ref {
                            return Err("activation tab seed differs from exported View".into());
                        }
                        if export.mode == UiTransferExportMode::Move {
                            tile.local = source.local.clone();
                        }
                    }
                }
                for (set, tab) in installed.into_iter().zip(&manifest.tabs) {
                    let bindings = tab
                        .tile_ids
                        .iter()
                        .map(|tile| {
                            let instance = RyeOsViewInstanceKey::surface_tile(
                                &reservation.presentation_id,
                                &manifest.surface_instance_id,
                                *tile,
                            );
                            let binding = reservation
                                .mounts
                                .iter()
                                .find(|mount| mount.destination_instance == instance)
                                .ok_or("activation tab tile has no reserved binding")?;
                            Ok((*tile, binding.binding.clone()))
                        })
                        .collect::<Result<BTreeMap<_, _>, String>>()?;
                    let index = staged.composition.surfaces()[&manifest.surface_instance_id]
                        .view_sets()
                        .len();
                    staged.composition.insert_view_set(
                        &manifest.surface_instance_id,
                        set,
                        index,
                        bindings,
                        false,
                    )?;
                }
                if let Some(active) = active_view_set_id {
                    staged
                        .composition
                        .select_view_set(&manifest.surface_instance_id, *active)?;
                }
                for tab in &manifest.tabs {
                    staged
                        .insertion_bindings
                        .insert(tab.view_set_id, tab.binding.clone());
                    staged
                        .template_ids
                        .insert(tab.view_set_id, tab.seed.id.clone());
                }
            }
            (
                UiTransferDestinationPlacement::View {
                    surface_instance_id,
                    view_set_id,
                    allocation,
                    placement,
                },
                UiTransferExportContent::SurfaceMount { mount, .. },
            ) => {
                let destination = &reservation.mounts[0];
                let tile = allocation.tile_ids[0];
                let group = allocation.group_ids.first().copied();
                let view = crate::surface::ViewKindSpec(mount.view_ref.clone()).to_view_spec();
                let local = if export.mode == UiTransferExportMode::Copy {
                    view.initial_local_state()
                } else {
                    mount.local.clone()
                };
                let instance = destination.destination_instance.clone();
                let binding = destination.binding.clone();
                staged.composition.mutate_view_set(
                    surface_instance_id,
                    *view_set_id,
                    BTreeMap::from([(tile, binding)]),
                    |set| {
                        super::composition_delta::place(&mut set.root, tile, placement, group)?;
                        if set
                            .tiles
                            .insert(
                                tile,
                                crate::view_set::TileState {
                                    instance_key: instance,
                                    local,
                                    view,
                                },
                            )
                            .is_some()
                        {
                            return Err("activation destination tile already exists".into());
                        }
                        set.focus_tile(tile);
                        Ok(())
                    },
                )?;
            }
            (
                UiTransferDestinationPlacement::SharedDock { edge },
                UiTransferExportContent::SharedDock {
                    visible,
                    size,
                    mount,
                    ..
                },
            ) => {
                let destination = &reservation.mounts[0];
                staged.composition.install_shared_dock(
                    edge,
                    mount.view_ref.clone(),
                    destination.binding.clone(),
                    if export.mode == UiTransferExportMode::Copy {
                        crate::surface::ViewKindSpec(mount.view_ref.clone())
                            .to_view_spec()
                            .initial_local_state()
                    } else {
                        mount.local.clone()
                    },
                    *visible,
                    *size,
                )?;
                if staged
                    .composition
                    .mount(&destination.destination_instance)
                    .is_none()
                {
                    return Err("activated shared dock differs from reserved instance".into());
                }
            }
            _ => return Err("activation placement differs from signed export kind".into()),
        }
        staged.reconcile_subject_mounts()?;
        for (source, destination) in source_mounts.into_iter().zip(&reservation.mounts) {
            let descriptor = staged
                .descriptors
                .get(&destination.binding.binding_attachment_id)
                .filter(|descriptor| coordinate(descriptor) == destination.binding)
                .ok_or("activated mount lacks exact descriptor")?;
            if !descriptor.presentation.views.contains_key(&source.view_ref) {
                return Err("activated View is outside destination closure".into());
            }
            validate_transfer_subject(
                &destination.destination_subject,
                descriptor,
                &source.view_ref,
                &destination.destination_instance,
                &staged.composition,
                max_bytes,
            )?;
            let required_input = match &destination.destination_subject {
                super::attachment::SelectionAttachment::RequiredSubject { input, .. } => {
                    Some(input.clone())
                }
                _ => None,
            };
            staged.subjects.insert(
                destination.destination_instance.clone(),
                UiCompositionMountSubject {
                    binding: destination.binding.clone(),
                    view_ref: source.view_ref.clone(),
                    required_input,
                    attachment: destination.destination_subject.clone(),
                },
            );
            if let super::composition_operation::UiCompositionRouteAssignment::Override {
                input_route,
            } = &source.route
            {
                staged.input_routes.insert(
                    destination.destination_instance.clone(),
                    (activated.evidence.chain_seq, input_route.clone()),
                );
            }
            if source.current_label.is_some() {
                staged.lens_labels.insert(
                    destination.destination_instance.clone(),
                    source.current_label.clone(),
                );
            }
            let mut frames = Vec::new();
            for (source_frame, destination_frame) in
                source.lens_frames.iter().zip(&destination.lens_frames)
            {
                let descriptor = staged
                    .descriptors
                    .get(&destination_frame.binding.binding_attachment_id)
                    .filter(|descriptor| coordinate(descriptor) == destination_frame.binding)
                    .ok_or("activated lens frame lacks exact descriptor")?;
                if !descriptor
                    .presentation
                    .views
                    .contains_key(&source_frame.view_ref)
                {
                    return Err("activated lens View is outside destination closure".into());
                }
                validate_transfer_subject(
                    &destination_frame.destination_subject,
                    descriptor,
                    &source_frame.view_ref,
                    &destination.destination_instance,
                    &staged.composition,
                    max_bytes,
                )?;
                let frame = super::composition_lens::RetainedLensFrame {
                    binding: destination_frame.binding.clone(),
                    view_ref: source_frame.view_ref.clone(),
                    attachment: destination_frame.destination_subject.clone(),
                    input_route: source_frame.input_route.clone(),
                    label: source_frame.label.clone(),
                };
                frame.validate_shape(
                    max_bytes.min(descriptor.binding_request_bounds.max_request_bytes),
                )?;
                if let Some(route) = frame.input_route.as_ref() {
                    serde_json::from_value::<super::seat::InputRoute>(route.clone())
                        .map_err(|_| "activated lens frame route is malformed")?;
                }
                frames.push(frame);
            }
            if serde_json::to_vec(&frames)
                .map_err(|error| error.to_string())?
                .len() as u64
                > max_bytes
            {
                return Err("activated lens stack exceeds destination request bound".into());
            }
            if !frames.is_empty() {
                staged
                    .lens_frames
                    .insert(destination.destination_instance.clone(), frames);
            }
            if export.mode == UiTransferExportMode::Move {
                for draft in &source.drafts {
                    let fresh = mapped_transfer_draft_binding(
                        &draft.key,
                        std::iter::once((
                            source.view_ref.as_str(),
                            &source.binding,
                            &destination.binding,
                        ))
                        .chain(
                            source.lens_frames.iter().zip(&destination.lens_frames).map(
                                |(frame, allocated)| {
                                    (frame.view_ref.as_str(), &frame.binding, &allocated.binding)
                                },
                            ),
                        ),
                    )?;
                    let mut key = draft.key.clone();
                    key.view_instance_key = destination.destination_instance.clone();
                    key.binding_scope = super::model::InputBufferKey::binding_scope(fresh);
                    let descriptor = staged
                        .descriptors
                        .get(&fresh.binding_attachment_id)
                        .filter(|descriptor| coordinate(descriptor) == *fresh)
                        .ok_or("activated draft has no exact destination descriptor")?;
                    draft.state.validate_retained_binding(&key, descriptor)?;
                    if !Arc::make_mut(&mut staged.transfer_draft_keys).insert(key.clone())
                        || staged
                            .transfer_drafts
                            .insert(key, draft.state.clone())
                            .is_some()
                    {
                        return Err(
                            "activated draft collides with retained destination scope".into()
                        );
                    }
                }
            }
        }
        staged.accepted_transfers.remove(&transfer.operation_id);
        Arc::make_mut(&mut staged.settled).insert(transfer.operation_id.clone());
        staged.advance_revision()?;
        staged.last_chain_seq = Some(activated.evidence.chain_seq);
        *self = staged;
        Ok(())
    }

    /// Fold one exact scheduled transfer row after the caller has separately
    /// authenticated its canonical CAS bytes and completed paired cut. This
    /// type-level lifecycle check is consistency only, never proof authority.
    pub fn apply_authenticated_transfer_phase(
        &mut self,
        lifecycle: &ryeos_ui_presentation_contract::transfer_state::TransferLifecycleState,
        record: &ryeos_ui_presentation_contract::transfer::TransferRecord,
        evidence: &ryeos_ui_presentation_contract::transfer::TransferEvidence,
        max_bytes: u64,
    ) -> Result<(), String> {
        use ryeos_ui_presentation_contract::transfer::TransferPhase;
        let testimony = match &record.phase {
            TransferPhase::InboundCommitment => Some(lifecycle.inbound()),
            TransferPhase::OrphanCancelled { .. } => lifecycle.orphan_cancelled(),
            TransferPhase::Prepared { .. } => lifecycle.prepared(),
            TransferPhase::Accepted { .. } => lifecycle.accepted(),
            TransferPhase::Committed { .. } => lifecycle.committed(),
            TransferPhase::Aborted { .. } => lifecycle.aborted(),
            TransferPhase::Activated { .. } => lifecycle.activated(),
            TransferPhase::Retained { .. } => lifecycle.retained(),
            TransferPhase::Released { .. } => lifecycle.released(),
            TransferPhase::SettlementObserved { .. } => lifecycle.settlement_observed(),
        }
        .ok_or("transfer phase absent from completed lifecycle")?;
        if testimony.record != record
            || testimony.evidence != evidence
            || lifecycle.transfer() != &record.transfer
            || record.event_owner().presentation_id != self.composition.id().as_str()
            || self
                .last_chain_seq
                .is_none_or(|previous| evidence.chain_seq <= previous)
        {
            return Err("transfer phase differs from exact causal presentation position".into());
        }
        match &record.phase {
            TransferPhase::InboundCommitment | TransferPhase::OrphanCancelled { .. } => {
                self.last_chain_seq = Some(evidence.chain_seq);
            }
            TransferPhase::Prepared { .. } => {
                let export =
                    super::transfer_export::UiTransferExport::from_preparation(record, max_bytes)?;
                if export.mode == super::transfer_export::UiTransferExportMode::Move
                    && !self.move_freezes.iter().any(|(id, freeze)| {
                        freeze.transfer == record.transfer
                            && freeze.export == export
                            && self.settled.contains(id)
                    })
                {
                    return Err("MOVE preparation has no exact settled source freeze".into());
                }
                self.last_chain_seq = Some(evidence.chain_seq);
            }
            TransferPhase::Accepted { .. } => {
                self.apply_authenticated_transfer_acceptance(lifecycle, max_bytes)?
            }
            TransferPhase::Activated { .. } => {
                self.apply_authenticated_transfer_activation(lifecycle, max_bytes)?
            }
            TransferPhase::Retained { .. } | TransferPhase::Released { .. } => {
                self.apply_authenticated_transfer_nonactivation(lifecycle, max_bytes)?
            }
            TransferPhase::Committed { .. } => {
                let prepared = lifecycle.prepared().ok_or("commit has no preparation")?;
                let export = super::transfer_export::UiTransferExport::from_preparation(
                    prepared.record,
                    max_bytes,
                )?;
                if export.mode == super::transfer_export::UiTransferExportMode::Move {
                    self.apply_authenticated_move_commit(lifecycle, max_bytes)?;
                } else {
                    self.last_chain_seq = Some(evidence.chain_seq);
                }
            }
            TransferPhase::Aborted { .. } => {
                let prepared = lifecycle.prepared().ok_or("abort has no preparation")?;
                let export = super::transfer_export::UiTransferExport::from_preparation(
                    prepared.record,
                    max_bytes,
                )?;
                if export.mode == super::transfer_export::UiTransferExportMode::Move {
                    self.apply_authenticated_move_abort(lifecycle, max_bytes)?;
                } else {
                    self.last_chain_seq = Some(evidence.chain_seq);
                }
            }
            TransferPhase::SettlementObserved { .. } => {
                let prepared = lifecycle
                    .prepared()
                    .ok_or("observation has no preparation")?;
                let export = super::transfer_export::UiTransferExport::from_preparation(
                    prepared.record,
                    max_bytes,
                )?;
                if export.mode == super::transfer_export::UiTransferExportMode::Move {
                    self.apply_authenticated_move_settlement_observed(lifecycle, max_bytes)?;
                } else {
                    self.last_chain_seq = Some(evidence.chain_seq);
                }
            }
        }
        if self.last_chain_seq != Some(evidence.chain_seq) {
            return Err("transfer reducer did not advance exact phase position".into());
        }
        self.paired_transfer_seen = true;
        Ok(())
    }
    pub fn begin(
        initial: &UiPresentationInitialization,
        first: &SeatEvent,
    ) -> Result<Self, String> {
        let SeatEventKind::PresentationInitialized(record) = &first.kind else {
            return Err("initialization must precede structural seat history".into());
        };
        record.validate()?;
        if &record.initialization != initial {
            return Err("replay testimony differs from the original initialization".into());
        }
        let mut replay = Self::initialized(initial)?;
        replay.last_chain_seq = Some(first.seq);
        Ok(replay)
    }
    /// Page readers fold incrementally. A rejected event leaves this projection intact.
    pub fn apply_ordered_event(&mut self, event: &SeatEvent) -> Result<(), String> {
        let staged = self.clone().apply_ordered_event_owned(event)?;
        *self = staged;
        Ok(())
    }

    /// Pure projection reduction for the daemon's separately authenticated
    /// paired cancellation lane. This is not a Core release grant: the owner
    /// must first authenticate the exact record, hashes, source cut and paired
    /// absence proof. Ordinary retained-event decoding remains sealed.
    pub fn apply_authenticated_move_freeze_cancellation(
        &mut self,
        cancellation: &ryeos_ui_presentation_contract::move_freeze_lifecycle::MoveFreezeCancellation,
        preparation: &SeatEvent,
        completion: Option<&SeatEvent>,
        chain_seq: u64,
    ) -> Result<(), String> {
        cancellation.validate_shape()?;
        let SeatEventKind::PresentationOperationPrepared(operation) = &preparation.kind else {
            return Err("cancellation preparation has another event kind".into());
        };
        operation.validate().map_err(str::to_owned)?;
        let request = operation
            .move_freeze()
            .ok_or("cancellation has no typed freeze")?;
        if let Some(completion) = completion {
            let SeatEventKind::PresentationOperationCompleted(completed) = &completion.kind else {
                return Err("cancellation completion has another event kind".into());
            };
            completed.validate_for(operation).map_err(str::to_owned)?;
        }
        let completion_seq = completion.map(|event| event.seq);
        let phase_matches = if completion.is_some() {
            self.settled.contains(&operation.operation_id)
                && !self.pending.contains_key(&operation.operation_id)
        } else {
            !self.settled.contains(&operation.operation_id)
                && self.pending.get(&operation.operation_id) == Some(operation)
        };
        let root = self
            .descriptors
            .get(&self.initialization.root_binding.binding_attachment_id)
            .filter(|descriptor| coordinate(descriptor) == self.initialization.root_binding)
            .ok_or("cancellation retained root descriptor absent")?;
        if request.transfer != cancellation.transfer
            || request.root_binding != self.initialization.root_binding
            || operation.request_bounds != root.binding_request_bounds
            || self.move_freezes.get(&operation.operation_id) != Some(request)
            || !phase_matches
            || self.move_freeze_positions.get(&operation.operation_id)
                != Some(&(preparation.seq, completion_seq))
            || preparation.seq != cancellation.freeze.preparation.chain_seq
            || completion_seq
                != cancellation
                    .freeze
                    .completion
                    .as_ref()
                    .map(|evidence| evidence.chain_seq)
            || chain_seq <= completion_seq.unwrap_or(preparation.seq)
            || self
                .last_chain_seq
                .is_none_or(|previous| chain_seq <= previous)
        {
            return Err("cancellation differs from exact settled retained freeze".into());
        }
        self.move_freezes.remove(&operation.operation_id);
        self.move_freeze_positions.remove(&operation.operation_id);
        self.pending.remove(&operation.operation_id);
        Arc::make_mut(&mut self.settled).insert(operation.operation_id.clone());
        self.last_chain_seq = Some(chain_seq);
        self.paired_transfer_seen = true;
        Ok(())
    }

    /// Consume a private staged projection. On refusal the whole projection is
    /// discarded by the caller, so no history-sized rollback copy is needed.
    /// Never use this on the published cache without taking exclusive ownership
    /// and blocking publication if it fails.
    pub fn apply_ordered_event_owned(mut self, event: &SeatEvent) -> Result<Self, String> {
        if self
            .last_chain_seq
            .is_none_or(|previous| event.seq <= previous)
        {
            return Err("structural history is not in exact increasing chain order".into());
        }
        self.validate_move_freeze_event(event)?;
        if let SeatEventKind::Facet { key, value } = &event.kind
            && let Some(draft_key) = super::model::InputBufferKey::from_storage_key(key)
            && self.transfer_draft_keys.contains(&draft_key)
        {
            if value.is_null() {
                self.transfer_drafts.remove(&draft_key);
            } else {
                let text = value
                    .as_str()
                    .ok_or("retained transfer draft facet is not text")?;
                let mut state = self
                    .transfer_drafts
                    .get(&draft_key)
                    .cloned()
                    .unwrap_or_default();
                state.set_text(text.to_owned(), text.len());
                let descriptor = self
                    .descriptors
                    .values()
                    .find(|descriptor| {
                        draft_key.binding_scope
                            == super::model::InputBufferKey::binding_scope(&coordinate(descriptor))
                    })
                    .ok_or("retained transfer draft has no exact descriptor")?;
                state.validate_retained_binding(&draft_key, descriptor)?;
                self.transfer_drafts.insert(draft_key, state);
            }
            self.last_chain_seq = Some(event.seq);
            return Ok(self);
        }
        if matches!(&event.kind, SeatEventKind::Facet { key, .. }
            if key != KEY_ACTIVE_SURFACE && !key.starts_with(&format!("{}\u{1f}", super::seat::KEY_INPUT_ROUTE)))
        {
            self.last_chain_seq = Some(event.seq);
            return Ok(self);
        }
        self.apply_checked_event(event)?;
        self.last_chain_seq = Some(event.seq);
        Ok(self)
    }
    /// Freeze guards apply before the ordinary-facet fast path as well as
    /// structural replay. They protect exact retained inventory, not the seat.
    fn validate_move_freeze_event(&self, event: &SeatEvent) -> Result<(), String> {
        use super::composition_operation::CompositionMutation;
        use super::transfer_export::UiTransferExportContent;
        use crate::ids::UiViewMountOwner;
        for freeze in self.move_freezes.values() {
            let instances = freeze.instances.iter().cloned().collect::<BTreeSet<_>>();
            let sets = freeze.view_sets.iter().copied().collect::<BTreeSet<_>>();
            let blocked = match &event.kind {
                SeatEventKind::PresentationOperationPrepared(operation) => {
                    if let Some(admission) = operation.admission() {
                        instances.contains(&admission.invocation_origin)
                            || admission.surface_open_target.as_ref().is_some_and(|target| {
                                matches!(&target.placement,crate::surface::opening::SurfaceOpenPlacement::AppendTabs {destination_surface_instance_id,activate_imported_tab:true,..}
                                    if matches!(&freeze.export.content,UiTransferExportContent::ViewSets{sets,..}
                                        if sets.iter().any(|set|matches!(&set.source_owner,UiViewMountOwner::Surface{surface_instance_id,..} if surface_instance_id==destination_surface_instance_id))))
                            })
                    } else if let Some(request) = operation.composition() {
                        // Shared dock freezes do not freeze other shared mounts.
                        match &request.mutation {
                            CompositionMutation::SetSubject { mount, .. }
                            | CompositionMutation::FocusView { mount } => instances.contains(mount),
                            _ => super::transfer_export::pending_request_affects_export(
                                request,
                                &instances,
                                &sets,
                                &freeze.export,
                            ),
                        }
                    } else {
                        false
                    }
                }
                SeatEventKind::SurfaceClosed(closure) => {
                    let owns_surface = |owner: &UiViewMountOwner| {
                        matches!(owner,
                        UiViewMountOwner::Surface { surface_instance_id, .. }
                            if surface_instance_id.as_str() == closure.surface_instance_id)
                    };
                    match &freeze.export.content {
                        UiTransferExportContent::ViewSets { sets, .. } => {
                            sets.iter().any(|set| owns_surface(&set.source_owner))
                        }
                        UiTransferExportContent::SurfaceMount { mount, .. } => mount
                            .source_instance
                            .mount_owner()
                            .is_some_and(|owner| owns_surface(&owner)),
                        UiTransferExportContent::SharedDock { .. } => false,
                    }
                }
                SeatEventKind::Facet { key, .. } => {
                    let route_instance = key
                        .strip_prefix(&format!("{}\u{1f}", super::seat::KEY_INPUT_ROUTE))
                        .and_then(RyeOsViewInstanceKey::from_canonical);
                    let draft_instance = super::model::InputBufferKey::from_storage_key(key)
                        .map(|key| key.view_instance_key);
                    route_instance
                        .or(draft_instance)
                        .is_some_and(|instance| instances.contains(&instance))
                        || sets.iter().any(|set| {
                            let selection = super::seat::selection_facet_key(*set);
                            key == &selection
                                || key
                                    .strip_prefix(&selection)
                                    .is_some_and(|suffix| suffix.starts_with('.'))
                        })
                }
                _ => false,
            };
            if blocked {
                return Err("retained mutation crosses exact MOVE freeze inventory".into());
            }
        }
        Ok(())
    }
    fn apply_checked_event(&mut self, event: &SeatEvent) -> Result<(), String> {
        match &event.kind {
            SeatEventKind::PresentationInitialized(_) => {
                return Err("structural history contains duplicate initialization".into());
            }
            SeatEventKind::PresentationOperationPrepared(operation) => {
                operation.validate().map_err(str::to_owned)?;
                if operation.producer.presentation_id != self.initialization.presentation_id
                    || operation.root_binding != self.initialization.root_binding
                    || self.pending.contains_key(&operation.operation_id)
                    || self.settled.contains(&operation.operation_id)
                {
                    return Err("preparation contradicts causal presentation identity".into());
                }
                if !self.pending.is_empty() {
                    return Err("preparation follows an unresolved structural operation".into());
                }
                // Historical ownership is checked at this exact point,
                // never against today's membership after later closure.
                if let Some(admission) = operation.admission() {
                    let mount = self
                        .composition
                        .mount(&admission.invocation_origin)
                        .ok_or("preparation origin is not an existing mounted View")?;
                    let request_coordinate = UiBindingAttachmentCoordinate {
                        binding_attachment_id: admission.request.binding_attachment_id.clone(),
                        binding_generation: admission.request.binding_generation,
                        binding_digest: admission.request.binding_digest.clone(),
                    };
                    if mount.binding() != &request_coordinate {
                        return Err("preparation borrows another mount's binding".into());
                    }
                    match &admission.request.coordinate {
                        super::binding::UiBindingCoordinate::Source { view_ref, .. }
                        | super::binding::UiBindingCoordinate::Affordance { view_ref, .. }
                            if view_ref == mount.view_ref() => {}
                        _ => {
                            return Err(
                                "preparation borrows another mounted View coordinate".into()
                            );
                        }
                    }
                } else if let Some(request) = operation.move_freeze() {
                    let admitted_bounds = self
                        .descriptors
                        .get(&self.initialization.root_binding.binding_attachment_id)
                        .filter(|descriptor| {
                            coordinate(descriptor) == self.initialization.root_binding
                        })
                        .ok_or("MOVE freeze retained root descriptor absent")?
                        .binding_request_bounds;
                    if operation.request_bounds != admitted_bounds {
                        return Err("MOVE freeze changes the retained root request bounds".into());
                    }
                    request.export.validate_snapshot(
                        &request.transfer,
                        self,
                        operation.request_bounds.max_request_bytes,
                    )?;
                    if request.root_binding != self.initialization.root_binding
                        || self.settled.contains(&request.transfer.operation_id)
                        || self.committed_move_freezes.values().any(|(prior, _)| {
                            prior.transfer.operation_id == request.transfer.operation_id
                        })
                        || self.move_freezes.values().any(|prior| {
                            prior.transfer.operation_id == request.transfer.operation_id
                                || prior
                                    .instances
                                    .iter()
                                    .any(|key| request.instances.contains(key))
                                || prior
                                    .view_sets
                                    .iter()
                                    .any(|id| request.view_sets.contains(id))
                        })
                    {
                        return Err("MOVE freeze overlaps a retained obligation or root".into());
                    }
                    if !super::admission_operation::single_seat_append_fits(
                        &operation.producer,
                        super::admission_operation::PREPARED_EVENT,
                        operation,
                        operation.request_bounds.max_seat_append_bytes,
                    ) {
                        return Err("MOVE freeze exceeds its retained append bound".into());
                    }
                    let completion = super::admission_operation::UiCompletedPresentationOperation {
                        operation_id: operation.operation_id.clone(),
                        result: serde_json::to_value(request.completion())
                            .map_err(|error| error.to_string())?,
                    };
                    if !super::admission_operation::single_seat_append_fits(
                        &operation.producer,
                        super::admission_operation::COMPLETED_EVENT,
                        &completion,
                        operation.request_bounds.max_seat_append_bytes,
                    ) {
                        return Err(
                            "MOVE freeze completion exceeds its retained append bound".into()
                        );
                    }
                    self.move_freezes
                        .insert(operation.operation_id.clone(), request.clone());
                    self.move_freeze_positions
                        .insert(operation.operation_id.clone(), (event.seq, None));
                } else if operation.composition().is_some() {
                    self.expected_composition_completion(
                        operation,
                        operation.request_bounds.max_request_bytes,
                    )?;
                    if !super::admission_operation::single_seat_append_fits(
                        &operation.producer,
                        super::admission_operation::PREPARED_EVENT,
                        operation,
                        operation.request_bounds.max_seat_append_bytes,
                    ) {
                        return Err(
                            "composition preparation exceeds its retained append bound".into()
                        );
                    }
                }
                self.pending
                    .insert(operation.operation_id.clone(), operation.clone());
            }
            SeatEventKind::PresentationOperationCompleted(completed) => {
                let prepared = self
                    .pending
                    .get(&completed.operation_id)
                    .ok_or("completion has no prior exact preparation")?;
                completed.validate_for(prepared).map_err(str::to_owned)?;
                if prepared.move_freeze().is_some() {
                    if !self.move_freezes.contains_key(&completed.operation_id) {
                        return Err("MOVE freeze completion has no retained obligation".into());
                    }
                    self.pending.remove(&completed.operation_id);
                    self.move_freeze_positions
                        .get_mut(&completed.operation_id)
                        .ok_or("MOVE freeze completion has no preparation position")?
                        .1 = Some(event.seq);
                    Arc::make_mut(&mut self.settled).insert(completed.operation_id.clone());
                    return Ok(());
                }
                if let Some(request) = prepared.composition() {
                    let (delta, lens_transition) =
                        self.stage_delta(request, prepared.request_bounds.max_request_bytes)?;
                    let replaced_instance =
                        if let super::composition_operation::CompositionMutation::ReplaceView {
                            tab_id,
                            tile_id,
                            ..
                        } = &request.mutation
                        {
                            request.owner.surface_instance_id().and_then(|owner| {
                                self.composition
                                    .view_set(owner, *tab_id)
                                    .and_then(|set| set.tiles.get(tile_id))
                                    .map(|tile| tile.instance_key.clone())
                            })
                        } else {
                            None
                        };
                    let expected =
                        super::composition_completion::UiCompositionCompletion::from_delta(
                            request, &delta,
                        )?;
                    let received: super::composition_completion::UiCompositionCompletion =
                        serde_json::from_value(completed.result.clone())
                            .map_err(|error| error.to_string())?;
                    if received != expected {
                        return Err(
                            "composition completion differs from its exact staged delta".into()
                        );
                    }
                    if let Some(instance) = replaced_instance {
                        self.lens_frames.remove(&instance);
                        self.input_routes.remove(&instance);
                        self.lens_labels.remove(&instance);
                    }
                    if let Some(manifest) = delta.allocated_manifest.as_ref() {
                        for tab in &manifest.tabs {
                            self.insertion_bindings
                                .insert(tab.view_set_id, tab.binding.clone());
                            self.template_ids
                                .insert(tab.view_set_id, tab.seed.id.clone());
                        }
                    }
                    self.remap_completed_mount_metadata(&delta)?;
                    if !delta.mount_remaps.is_empty()
                        && self
                            .completed_mount_remaps
                            .insert(completed.operation_id.clone(), delta.mount_remaps.clone())
                            .is_some()
                    {
                        return Err("completed composition remap repeats an operation".into());
                    }
                    self.composition = delta.composition;
                    self.revision = delta.revision;
                    self.next_mount_id = delta.next_mount_id;
                    self.reconcile_subject_mounts()?;
                    if let Some((instance, frames, route, label)) = lens_transition {
                        self.lens_frames.insert(instance.clone(), frames);
                        self.lens_labels.insert(instance.clone(), label);
                        self.input_routes.insert(instance, (event.seq, route));
                    }
                    for assignment in delta.subjects {
                        use super::composition_delta::UiCompositionSubjectAssignment;
                        use crate::surface::view_sets::SavedViewSelectionSource;
                        let (instance_key, attachment) = match assignment {
                            UiCompositionSubjectAssignment::Exact {
                                instance_key,
                                attachment,
                            } => (instance_key, attachment),
                            UiCompositionSubjectAssignment::Portable {
                                instance_key,
                                view_set_id,
                                resolved_follow_set,
                                source,
                            } => {
                                let attachment = match source {
                                    SavedViewSelectionSource::FollowOwnSet => {
                                        super::attachment::SelectionAttachment::FollowViewSet {
                                            view_set_id,
                                        }
                                    }
                                    SavedViewSelectionSource::FollowSet { .. } => {
                                        super::attachment::SelectionAttachment::FollowViewSet {
                                            view_set_id: resolved_follow_set.ok_or(
                                                "composition FollowSet has no resolved owner",
                                            )?,
                                        }
                                    }
                                    SavedViewSelectionSource::RequiredSubject { input, facets } => {
                                        super::attachment::SelectionAttachment::RequiredSubject {
                                            input,
                                            facets,
                                        }
                                    }
                                };
                                (instance_key, attachment)
                            }
                        };
                        let mount = self
                            .composition
                            .mount(&instance_key)
                            .ok_or("subject assignment mount absent")?;
                        self.subjects
                            .entry(instance_key.clone())
                            .or_insert_with(|| UiCompositionMountSubject {
                                binding: mount.binding().clone(),
                                view_ref: mount.view_ref().into(),
                                required_input: None,
                                attachment: attachment.clone(),
                            });
                        let subject = self
                            .subjects
                            .get_mut(&instance_key)
                            .ok_or("composition subject assignment has no mounted payload")?;
                        subject.required_input = match &attachment {
                            super::attachment::SelectionAttachment::RequiredSubject {
                                input,
                                ..
                            } => Some(input.clone()),
                            _ => None,
                        };
                        subject.attachment = attachment;
                    }
                    self.pending.remove(&completed.operation_id);
                    Arc::make_mut(&mut self.settled).insert(completed.operation_id.clone());
                    return Ok(());
                }
                if prepared.admission().is_some_and(|admission|admission.purpose==super::admission_operation::UiPresentationOperationKind::OpenPresentationDocument) {
                    // The child initialization is authenticated by completion validation.
                    // It never contributes mounts, attachments or producer ownership here.
                    self.pending.remove(&completed.operation_id);
                    Arc::make_mut(&mut self.settled).insert(completed.operation_id.clone());
                    return Ok(());
                }
                if prepared.admission().is_some_and(|admission|admission.purpose==super::admission_operation::UiPresentationOperationKind::ImportSurfaceTabs) {
                    let transition=serde_json::from_value(completed.result.get("ui_transition").cloned().ok_or("import transition absent")?).map_err(|error|error.to_string())?;
                    self.apply_import_transition(transition,prepared.request_bounds.max_request_bytes)?;
                    self.pending.remove(&completed.operation_id);
                    Arc::make_mut(&mut self.settled).insert(completed.operation_id.clone());
                    return Ok(());
                }
                let transition: AdmissionTransition = serde_json::from_value(
                    completed
                        .result
                        .get("ui_transition")
                        .cloned()
                        .ok_or("admission completion has no transition")?,
                )
                .map_err(|error| error.to_string())?;
                if (transition.kind == "open_surface") != transition.opening.is_some()
                    || transition.opening.as_ref().is_some_and(|opening| {
                        !matches!(
                            opening.placement,
                            crate::surface::opening::SurfaceOpenPlacement::NewSurface
                        )
                    })
                {
                    return Err("new Surface transition changes its opening placement".into());
                }
                if transition.presentation_id != self.initialization.presentation_id
                    || transition.creation_request_id != completed.operation_id
                    || coordinate(&transition.attachment) != transition.surface.binding
                    || self
                        .used_surfaces
                        .contains(&transition.surface.surface_instance_id)
                    || self.used_ordinals.contains(&transition.surface.ordinal)
                    || transition.surface.mount_manifest.allocation_start != self.next_mount_id
                {
                    return Err("completion contradicts causal admission or allocation".into());
                }
                let mut particular_plan = None;
                if transition.kind == "resume_particular_view_set" {
                    if transition.tab_attachments.is_some()
                        || transition.surface.saved_surface.is_some()
                    {
                        return Err(
                            "single particular admission contains saved-Surface content".into()
                        );
                    }
                    let resolved: crate::surface::view_sets::ResolvedParticularViewSet =
                        serde_json::from_value(
                            completed
                                .result
                                .get("particular_view_set")
                                .cloned()
                                .ok_or("particular set projection absent")?,
                        )
                        .map_err(|error| error.to_string())?;
                    self.composition.open_particular_surface(
                        &transition.surface,
                        &transition.attachment,
                        &resolved,
                    )?;
                    let id = &transition.surface.mount_manifest.surface_instance_id;
                    let set = self.composition.surfaces()[id]
                        .view_sets()
                        .first()
                        .ok_or("particular tab absent")?;
                    let ordered = set
                        .root
                        .as_ref()
                        .map(|root| root.tile_ids())
                        .unwrap_or_default()
                        .iter()
                        .map(|tile| {
                            let tile = set.tiles.get(tile).ok_or("particular mount absent")?;
                            Ok((tile.instance_key.clone(), tile.view.view_ref.clone()))
                        })
                        .collect::<Result<Vec<_>, String>>()?;
                    let surface_value = transition
                        .attachment
                        .presentation
                        .surface_value()
                        .map_err(|error| error.to_string())?
                        .ok_or("particular descriptor is not a Surface")?;
                    let views = super::content::views_from_surface(Some(&surface_value));
                    let template = crate::surface::view_sets::SavedViewSetTemplate {
                        id: resolved.id,
                        name: resolved.name,
                        composition: resolved.composition,
                        relationships: resolved.relationships,
                    };
                    particular_plan = Some((
                        set.id,
                        template.id.clone(),
                        super::composition_subjects::project_particular_subjects(
                            &template,
                            resolved.resolved_selection_work.as_ref(),
                            set.id,
                            &ordered,
                            &views,
                            transition
                                .attachment
                                .binding_request_bounds
                                .max_request_bytes,
                        )?,
                    ));
                } else if transition.kind == "resume_particular_surface"
                    || (transition.kind == "open_surface"
                        && transition.surface.saved_surface.is_some())
                {
                    let saved = transition
                        .surface
                        .saved_surface
                        .as_ref()
                        .ok_or("saved Surface completion has no exact saved composition")?;
                    match transition.kind.as_str() {
                        "resume_particular_surface" => {
                            if saved.particular_tabs().is_none()
                                || completed.result.get("particular_surface")
                                    != Some(
                                        &serde_json::to_value(saved)
                                            .map_err(|error| error.to_string())?,
                                    )
                            {
                                return Err(
                                    "particular Surface projection contradicts its transition"
                                        .into(),
                                );
                            }
                        }
                        "open_surface" => {
                            if saved.portable_tabs().is_none()
                                || completed.result.get("particular_surface").is_some()
                            {
                                return Err(
                                    "reusable Surface masquerades as particular resume".into()
                                );
                            }
                        }
                        _ => {
                            return Err("saved Surface has an unsupported admission purpose".into());
                        }
                    }
                    let mut descriptors = BTreeMap::new();
                    for descriptor in transition
                        .tab_attachments
                        .ok_or("saved Surface has no retained tab descriptors")?
                    {
                        descriptor.validate().map_err(str::to_owned)?;
                        if descriptors
                            .insert(descriptor.binding_attachment_id.clone(), descriptor)
                            .is_some()
                        {
                            return Err("saved Surface duplicates a tab descriptor".into());
                        }
                    }
                    let required = saved
                        .tab_bindings()
                        .into_iter()
                        .map(|binding| binding.binding_attachment_id.as_str())
                        .collect::<BTreeSet<_>>();
                    if required.len() != descriptors.len()
                        || saved.tab_bindings().into_iter().any(|binding| {
                            descriptors
                                .get(&binding.binding_attachment_id)
                                .is_none_or(|descriptor| &coordinate(descriptor) != binding)
                        })
                    {
                        return Err("saved Surface tab descriptor closure differs".into());
                    }
                    self.composition.open_saved_surface(
                        &transition.surface,
                        &transition.attachment,
                        &descriptors,
                    )?;
                    for descriptor in descriptors.into_values() {
                        self.retain_descriptor(descriptor)?;
                    }
                } else {
                    if transition
                        .tab_attachments
                        .as_ref()
                        .is_some_and(|tabs| !tabs.is_empty())
                        || transition.surface.saved_surface.is_some()
                    {
                        return Err("ordinary admission contains saved-Surface content".into());
                    }
                    self.composition
                        .open_surface(&transition.surface, &transition.attachment)?;
                }
                self.retain_descriptor(transition.attachment)?;
                for tab in &transition.surface.mount_manifest.tabs {
                    self.template_ids
                        .insert(tab.view_set_id, tab.seed.id.clone());
                }
                self.admitted_surfaces.insert(
                    transition.surface.surface_instance_id.clone(),
                    transition.surface.clone(),
                );
                self.next_mount_id = transition.surface.mount_manifest.next_mount_id;
                Arc::make_mut(&mut self.used_surfaces)
                    .insert(transition.surface.surface_instance_id);
                Arc::make_mut(&mut self.used_ordinals).insert(transition.surface.ordinal);
                self.reconcile_subject_mounts()?;
                if let Some((set_id, template_id, plan)) = particular_plan {
                    self.template_ids.insert(set_id, template_id);
                    self.insertion_bindings
                        .insert(set_id, transition.surface.binding.clone());
                    for (instance, subject) in plan.mounts {
                        let metadata = self
                            .subjects
                            .get_mut(&instance)
                            .ok_or("particular subject mount absent")?;
                        metadata.required_input = subject.required_input;
                        metadata.attachment = subject.attachment;
                    }
                    if let Some(work) = plan.work_selection {
                        self.work_selections.insert(set_id, work);
                    }
                }
                if transition.surface.saved_surface.is_some() {
                    self.hydrate_admitted_saved_subjects(
                        &transition.surface.mount_manifest.surface_instance_id,
                    )?;
                }
                self.pending.remove(&completed.operation_id);
                Arc::make_mut(&mut self.settled).insert(completed.operation_id.clone());
                self.advance_revision()?;
            }
            SeatEventKind::SurfaceClosed(closure) => {
                if !self.pending.is_empty() {
                    return Err("Surface closure follows an unresolved structural operation".into());
                }
                closure.validate().map_err(str::to_owned)?;
                let id =
                    crate::ids::SurfaceInstanceId::try_new(closure.surface_instance_id.clone())
                        .map_err(str::to_owned)?;
                if closure.presentation_id != self.initialization.presentation_id
                    || self
                        .composition
                        .surfaces()
                        .get(&id)
                        .is_none_or(|surface| surface.binding() != &closure.binding)
                {
                    return Err("closure has no exact live historical Surface".into());
                }
                self.composition
                    .close_surface(&id)
                    .ok_or("closure lost its historical Surface")?;
                self.admitted_surfaces.remove(&closure.surface_instance_id);
                Arc::make_mut(&mut self.closed_surfaces)
                    .insert(closure.surface_instance_id.clone(), closure.clone());
                self.reconcile_subject_mounts()?;
                self.advance_revision()?;
            }
            SeatEventKind::Facet { key, value } if key == KEY_ACTIVE_SURFACE => {
                if !self.pending.is_empty() {
                    return Err(
                        "Surface selection follows an unresolved structural operation".into(),
                    );
                }
                let selection: UiSurfaceSelection =
                    serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
                if self
                    .composition
                    .surfaces()
                    .get(&selection.surface_instance_id)
                    .is_none_or(|surface| surface.binding() != &selection.binding)
                {
                    return Err("Surface selection has no exact live historical owner".into());
                }
                self.composition
                    .select_surface(&selection.surface_instance_id)?;
                self.advance_revision()?;
            }
            SeatEventKind::Facet { key, value } => {
                if let Some(instance) = key
                    .strip_prefix(&format!("{}\u{1f}", super::seat::KEY_INPUT_ROUTE))
                    .and_then(RyeOsViewInstanceKey::from_canonical)
                    .filter(|instance| self.composition.mount(instance).is_some())
                {
                    for operation in self.pending.values() {
                        let Some(request) = operation.composition() else {
                            continue;
                        };
                        let target = match &request.mutation {
                            super::composition_operation::CompositionMutation::LensPush {
                                tab_id,
                                tile_id,
                                ..
                            }
                            | super::composition_operation::CompositionMutation::LensPop {
                                tab_id,
                                tile_id,
                            }
                            | super::composition_operation::CompositionMutation::ReplaceView {
                                tab_id,
                                tile_id,
                                ..
                            }
                            | super::composition_operation::CompositionMutation::ReplaceFollowingView {
                                tab_id,
                                tile_id,
                                ..
                            } => Some((*tab_id, *tile_id)),
                            super::composition_operation::CompositionMutation::MoveViewAcrossSurfaces {
                                source,
                                ..
                            } => Some((source.view_set_id, source.tile_id)),
                            _ => None,
                        };
                        if let Some((tab, tile)) = target {
                            let owner = request
                                .owner
                                .surface_instance_id()
                                .ok_or("pending replacement lacks Surface owner")?;
                            let addressed = self
                                .composition
                                .view_set(owner, tab)
                                .and_then(|set| set.tiles.get(&tile))
                                .ok_or("pending replacement mount is absent")?;
                            if addressed.instance_key == instance {
                                return Err(
                                    "input route write crosses an unsettled composition operation"
                                        .into(),
                                );
                            }
                        }
                    }
                    if !value.is_null() {
                        serde_json::from_value::<super::seat::InputRoute>(value.clone())
                            .map_err(|_| "mounted input route facet is malformed")?;
                    }
                    self.input_routes.insert(
                        instance,
                        (event.seq, (!value.is_null()).then(|| value.clone())),
                    );
                }
            }
        }
        Ok(())
    }

    /// Preflight the complete predicted receipt before accepting preparation.
    /// Authority and current bounds are supplied by the existing append owner.
    pub fn expected_composition_completion(
        &self,
        operation: &UiPreparedPresentationOperation,
        max_request_bytes: u64,
    ) -> Result<super::admission_operation::UiCompletedPresentationOperation, String> {
        let request = operation
            .composition()
            .ok_or("operation is not a composition request")?;
        let (delta, _) = self.stage_delta(
            request,
            max_request_bytes.min(operation.request_bounds.max_request_bytes),
        )?;
        let completion = super::admission_operation::UiCompletedPresentationOperation {
            operation_id: operation.operation_id.clone(),
            result: serde_json::to_value(
                super::composition_completion::UiCompositionCompletion::from_delta(
                    request, &delta,
                )?,
            )
            .map_err(|error| error.to_string())?,
        };
        completion.validate_for(operation).map_err(str::to_owned)?;
        Ok(completion)
    }

    pub fn affected_composition_bindings(
        &self,
        request: &super::composition_operation::UiCompositionRequest,
        max_request_bytes: u64,
    ) -> Result<Vec<UiBindingAttachmentCoordinate>, String> {
        let (delta, _) = self.stage_delta(request, max_request_bytes)?;
        Ok(delta.affected_bindings)
    }

    fn stage_delta(
        &self,
        request: &super::composition_operation::UiCompositionRequest,
        bound: u64,
    ) -> Result<
        (
            super::composition_delta::UiCompositionDelta,
            Option<(
                RyeOsViewInstanceKey,
                Vec<super::composition_lens::RetainedLensFrame>,
                Option<serde_json::Value>,
                Option<String>,
            )>,
        ),
        String,
    > {
        use super::composition_lens::RetainedLensFrame;
        use super::composition_operation::CompositionMutation;
        let (tab_id, tile_id, destination, push) = match &request.mutation {
            CompositionMutation::LensPush {
                tab_id,
                tile_id,
                view_ref,
                binding,
                attachment,
                input_route,
                label,
            } => (
                *tab_id,
                *tile_id,
                Some(RetainedLensFrame {
                    binding: binding.clone(),
                    view_ref: view_ref.clone(),
                    attachment: attachment.clone(),
                    input_route: input_route.clone(),
                    label: label.clone(),
                }),
                true,
            ),
            CompositionMutation::LensPop { tab_id, tile_id } => (*tab_id, *tile_id, None, false),
            _ => {
                if let CompositionMutation::MoveViewAcrossSurfaces { source, .. } =
                    &request.mutation
                {
                    let old = RyeOsViewInstanceKey::surface_tile(
                        self.composition.id(),
                        &source.surface_instance_id,
                        source.tile_id,
                    );
                    let subject = self
                        .subjects
                        .get(&old)
                        .ok_or("cross-Surface move has no exact retained subject")?;
                    if matches!(
                        &subject.attachment,
                        super::attachment::SelectionAttachment::FollowViewSet { .. }
                    ) || self.lens_frames.get(&old).is_some_and(|frames| {
                        frames.iter().any(|frame| {
                            matches!(
                                &frame.attachment,
                                super::attachment::SelectionAttachment::FollowViewSet { .. }
                            )
                        })
                    }) {
                        return Err(
                            "cross-Surface move has an unresolved current or frame FollowViewSet"
                                .into(),
                        );
                    }
                }
                let delta = super::composition_delta::stage_composition_delta(
                    &self.composition,
                    request,
                    self.revision,
                    self.next_mount_id,
                    &self.descriptors,
                    bound,
                )?;
                let target = match &request.mutation {
                    CompositionMutation::InsertView {
                        tab_id,
                        route_assignment,
                        ..
                    }
                    | CompositionMutation::InsertPinnedView {
                        tab_id,
                        route_assignment,
                        ..
                    }
                    | CompositionMutation::InsertFollowingView {
                        tab_id,
                        route_assignment,
                        ..
                    } => {
                        let tile = delta
                            .allocated_mounts
                            .as_ref()
                            .and_then(|allocation| allocation.tile_ids.first())
                            .copied()
                            .ok_or("routed insertion has no exact allocated tile")?;
                        Some((*tab_id, tile, route_assignment))
                    }
                    CompositionMutation::ReplaceView {
                        tab_id,
                        tile_id,
                        route_assignment,
                        ..
                    }
                    | CompositionMutation::ReplaceFollowingView {
                        tab_id,
                        tile_id,
                        route_assignment,
                        ..
                    } => Some((*tab_id, *tile_id, route_assignment)),
                    _ => None,
                };
                let transition = if let Some((tab, tile, assignment)) = target {
                    assignment.validate()?;
                    match assignment {
                        super::composition_operation::UiCompositionRouteAssignment::Authored => {
                            None
                        }
                        super::composition_operation::UiCompositionRouteAssignment::Override {
                            input_route,
                        } => {
                            let owner = request
                                .owner
                                .surface_instance_id()
                                .ok_or("routed View edit requires an exact Surface")?;
                            let instance = delta
                                .composition
                                .view_set(owner, tab)
                                .and_then(|set| set.tiles.get(&tile))
                                .ok_or("routed edit destination is not mounted")?
                                .instance_key
                                .clone();
                            Some((instance, Vec::new(), input_route.clone(), None))
                        }
                    }
                } else {
                    None
                };
                return Ok((delta, transition));
            }
        };
        let surface_id = request
            .owner
            .surface_instance_id()
            .ok_or("lens requires a Surface owner")?;
        let set = self
            .composition
            .view_set(surface_id, tab_id)
            .ok_or("lens tab is absent")?;
        if set.tiling.mode != crate::surface::TilingModeSpec::SingleLens {
            return Err("lens mutation requires an authored single-lens view set".into());
        }
        let tile = set.tiles.get(&tile_id).ok_or("lens tile is absent")?;
        let instance = tile.instance_key.clone();
        let mount = self
            .composition
            .mount(&instance)
            .ok_or("lens mount is absent")?;
        let mut frames = self.lens_frames.get(&instance).cloned().unwrap_or_default();
        let destination = if push {
            let metadata = self
                .subjects
                .get(&instance)
                .ok_or("lens source subject is absent")?;
            if metadata.binding != *mount.binding() || metadata.view_ref != mount.view_ref() {
                return Err("lens source subject differs from mounted payload".into());
            }
            let route = match self.input_routes.get(&instance) {
                Some((_, value)) => value.clone(),
                None => {
                    let descriptor = self
                        .descriptors
                        .get(&mount.binding().binding_attachment_id)
                        .ok_or("lens source descriptor absent")?;
                    let route = match &descriptor.presentation.definition {
                        crate::presentation::UiCompositionDefinition::Surface { surface } => {
                            super::seat::InputRoute::from_surface_input(surface.input.as_ref())
                                .unwrap_or_default()
                        }
                        _ => super::seat::InputRoute::default(),
                    };
                    Some(serde_json::to_value(route).map_err(|error| error.to_string())?)
                }
            };
            let frame = RetainedLensFrame {
                binding: mount.binding().clone(),
                view_ref: mount.view_ref().into(),
                attachment: metadata.attachment.clone(),
                input_route: route,
                label: self.lens_labels.get(&instance).cloned().flatten(),
            };
            frame.validate_shape(bound)?;
            frames.push(frame);
            destination.ok_or("lens destination absent")?
        } else {
            frames.pop().ok_or("lens return stack is empty")?
        };
        destination.validate_shape(bound)?;
        if let Some(route) = destination.input_route.as_ref() {
            serde_json::from_value::<super::seat::InputRoute>(route.clone())
                .map_err(|_| "lens destination input route is malformed")?;
        }
        if serde_json::to_vec(&frames)
            .map_err(|error| error.to_string())?
            .len() as u64
            > bound
        {
            return Err("retained lens stack exceeds binding byte bound".into());
        }
        let descriptor = self
            .descriptors
            .get(&destination.binding.binding_attachment_id)
            .filter(|descriptor| coordinate(descriptor) == destination.binding)
            .ok_or("lens destination exact descriptor absent")?;
        destination
            .validate_shape(bound.min(descriptor.binding_request_bounds.max_request_bytes))?;
        let view: super::content::ViewBinding = serde_json::from_value(
            descriptor
                .presentation
                .views
                .get(&destination.view_ref)
                .cloned()
                .ok_or("lens destination View absent")?,
        )
        .map_err(|error| error.to_string())?;
        let facets = super::attachment::selection_dependencies(&view)
            .into_iter()
            .collect::<Vec<_>>();
        let subject = if facets.is_empty() {
            crate::surface::view_sets::SavedViewSelectionSource::FollowOwnSet
        } else {
            crate::surface::view_sets::SavedViewSelectionSource::RequiredSubject {
                input: "lens".into(),
                facets,
            }
        };
        let mut replacement = request.clone();
        replacement.mutation = CompositionMutation::ReplaceView {
            tab_id,
            tile_id,
            view_ref: destination.view_ref.clone(),
            binding: destination.binding.clone(),
            subject,
            route_assignment: super::composition_operation::UiCompositionRouteAssignment::Authored,
        };
        let mut delta = super::composition_delta::stage_composition_delta(
            &self.composition,
            &replacement,
            self.revision,
            self.next_mount_id,
            &self.descriptors,
            bound,
        )?;
        let mut subject_request = request.clone();
        subject_request.expected_revision = delta.revision;
        subject_request.mutation = CompositionMutation::SetSubject {
            mount: instance.clone(),
            attachment: destination.attachment.clone(),
        };
        let subject_delta = super::composition_delta::stage_composition_delta(
            &delta.composition,
            &subject_request,
            delta.revision,
            delta.next_mount_id,
            &self.descriptors,
            bound,
        )?;
        for binding in subject_delta.affected_bindings {
            if !delta.affected_bindings.contains(&binding) {
                delta.affected_bindings.push(binding);
            }
        }
        delta.subjects = subject_delta.subjects;
        Ok((
            delta,
            Some((instance, frames, destination.input_route, destination.label)),
        ))
    }

    fn apply_import_transition(
        &mut self,
        transition: super::binding::UiSurfaceTabsAdmissionTransition,
        bound: u64,
    ) -> Result<(), String> {
        use super::composition_operation::{
            CompositionMutation, CompositionOwner, CompositionTabSeed, UiCompositionRequest,
        };
        use crate::surface::opening::SurfaceOpenPlacement;
        transition.validate()?;
        if transition.presentation_id != self.initialization.presentation_id {
            return Err("tab import belongs to another presentation".into());
        }
        let SurfaceOpenPlacement::AppendTabs {
            destination_surface_instance_id,
            destination_binding,
            expected_composition_revision,
            expected_next_mount_id,
            insertion_index,
            activate_imported_tab,
        } = &transition.opening.placement
        else {
            return Err("tab import has wrong placement".into());
        };
        let mut staged = self.clone();
        staged.retain_descriptor(transition.origin_attachment.clone())?;
        for descriptor in &transition.tab_attachments {
            staged.retain_descriptor(descriptor.clone())?;
        }
        let (tabs, active_seed_id) = if let Some(composition) = &transition.composition {
            (
                composition
                    .portable_tabs()
                    .ok_or("tab import cannot resume particular state")?
                    .iter()
                    .map(|tab| CompositionTabSeed {
                        template: tab.template.clone(),
                        binding: tab.binding.clone(),
                        fresh_subjects: tab.fresh_subjects.clone(),
                        followed_sets: BTreeMap::new(),
                        focused_view: None,
                    })
                    .collect::<Vec<_>>(),
                composition.active_view_set_id.clone(),
            )
        } else {
            let tabs = transition
                .manifest
                .tabs
                .iter()
                .map(|tab| CompositionTabSeed {
                    template: crate::surface::view_sets::SavedViewSetTemplate {
                        id: tab.seed.id.clone(),
                        name: tab.seed.title.clone(),
                        composition: tab.seed.clone(),
                        relationships: (0..tab.tile_ids.len()).map(|index| {
                            crate::surface::view_sets::SavedViewSelectionRelationship {
                                mount: crate::surface::view_sets::SavedViewMountRef::Tile { index },
                                source: crate::surface::view_sets::SavedViewSelectionSource::FollowOwnSet,
                            }
                        }).collect(),
                    },
                    binding: coordinate(&transition.origin_attachment),
                    fresh_subjects: BTreeMap::new(),
                    followed_sets: BTreeMap::new(),
                    focused_view: None,
                })
                .collect::<Vec<_>>();
            let active = tabs
                .first()
                .ok_or("authored import has no tabs")?
                .template
                .id
                .clone();
            (tabs, active)
        };
        let request = UiCompositionRequest {
            schema: 1,
            presentation_id: staged.composition.id().clone(),
            operation_id: transition.creation_request_id.clone(),
            expected_revision: *expected_composition_revision,
            expected_next_mount_id: *expected_next_mount_id,
            owner: CompositionOwner::Surface {
                surface_instance_id: destination_surface_instance_id.clone(),
                binding: destination_binding.clone(),
            },
            mutation: CompositionMutation::AppendTabs {
                index: *insertion_index,
                tabs,
                active_seed_id,
                activate: *activate_imported_tab,
            },
        };
        if staged.move_freezes.values().any(|freeze| {
            super::transfer_export::pending_request_affects_export(
                &request,
                &freeze.instances.iter().cloned().collect(),
                &freeze.view_sets.iter().copied().collect(),
                &freeze.export,
            )
        }) {
            return Err("tab import changes a retained MOVE inventory".into());
        }
        let (delta, _) = staged.stage_delta(&request, bound)?;
        if delta.allocated_manifest.as_ref() != Some(&transition.manifest) {
            return Err("import allocation differs from staged composition".into());
        }
        for tab in &transition.manifest.tabs {
            staged
                .insertion_bindings
                .insert(tab.view_set_id, tab.binding.clone());
            staged
                .template_ids
                .insert(tab.view_set_id, tab.seed.id.clone());
        }
        staged.composition = delta.composition;
        staged.revision = delta.revision;
        staged.next_mount_id = delta.next_mount_id;
        staged.reconcile_subject_mounts()?;
        for assignment in delta.subjects {
            use super::attachment::SelectionAttachment;
            use super::composition_delta::UiCompositionSubjectAssignment;
            use crate::surface::view_sets::SavedViewSelectionSource;
            let (instance, attachment) = match assignment {
                UiCompositionSubjectAssignment::Exact {
                    instance_key,
                    attachment,
                } => (instance_key, attachment),
                UiCompositionSubjectAssignment::Portable {
                    instance_key,
                    view_set_id,
                    resolved_follow_set,
                    source,
                } => {
                    let attachment = match source {
                        SavedViewSelectionSource::FollowOwnSet => {
                            SelectionAttachment::FollowViewSet { view_set_id }
                        }
                        SavedViewSelectionSource::FollowSet { .. } => {
                            SelectionAttachment::FollowViewSet {
                                view_set_id: resolved_follow_set
                                    .ok_or("import follower unresolved")?,
                            }
                        }
                        SavedViewSelectionSource::RequiredSubject { input, facets } => {
                            SelectionAttachment::RequiredSubject { input, facets }
                        }
                    };
                    (instance_key, attachment)
                }
            };
            let subject = staged
                .subjects
                .get_mut(&instance)
                .ok_or("import subject mount absent")?;
            subject.required_input = match &attachment {
                SelectionAttachment::RequiredSubject { input, .. } => Some(input.clone()),
                _ => None,
            };
            subject.attachment = attachment;
        }
        *self = staged;
        Ok(())
    }

    fn retain_descriptor(&mut self, descriptor: UiBindingAttachment) -> Result<(), String> {
        descriptor.validate().map_err(str::to_owned)?;
        if self
            .descriptors
            .get(&descriptor.binding_attachment_id)
            .is_some_and(|retained| retained != &descriptor)
        {
            return Err("admission changes an existing retained binding descriptor".into());
        }
        Arc::make_mut(&mut self.descriptors)
            .insert(descriptor.binding_attachment_id.clone(), descriptor);
        Ok(())
    }

    /// Reconcile only metadata for payloads that actually exist in this causal
    /// projection. Unchanged exact mounts retain their relationship, including
    /// pins and unresolved subjects; replacements cannot inherit an old pin.
    fn remap_completed_mount_metadata(
        &mut self,
        delta: &super::composition_delta::UiCompositionDelta,
    ) -> Result<(), String> {
        if delta.mount_remaps.is_empty() {
            return Ok(());
        }
        let mut destinations = BTreeSet::new();
        for (old, new) in &delta.mount_remaps {
            let source = self
                .composition
                .mount(old)
                .ok_or("remap source mount absent")?;
            let destination = delta
                .composition
                .mount(new)
                .ok_or("remap destination mount absent")?;
            if old == new
                || !destinations.insert(new.clone())
                || source.view_ref() != destination.view_ref()
                || source.binding() != destination.binding()
                || delta.composition.mount(old).is_some()
            {
                return Err("completed remap changes exact mounted View identity".into());
            }
            if !self.subjects.contains_key(old)
                || self.subjects.contains_key(new)
                || self.lens_frames.contains_key(new)
                || self.input_routes.contains_key(new)
                || self.lens_labels.contains_key(new)
            {
                return Err("completed remap has missing or colliding local metadata".into());
            }
        }
        let mut drafts = BTreeMap::new();
        for (key, value) in &self.transfer_drafts {
            let mut next = key.clone();
            if let Some(remapped) = delta.mount_remaps.get(&key.view_instance_key) {
                next.view_instance_key = remapped.clone();
            }
            if drafts.insert(next, value.clone()).is_some() {
                return Err("completed remap collides with retained draft".into());
            }
        }
        let mut draft_keys = BTreeSet::new();
        for key in self.transfer_draft_keys.iter() {
            let mut next = key.clone();
            if let Some(remapped) = delta.mount_remaps.get(&key.view_instance_key) {
                next.view_instance_key = remapped.clone();
            }
            if !draft_keys.insert(next) {
                return Err("completed remap collides with retained draft key".into());
            }
        }
        for (old, new) in &delta.mount_remaps {
            let subject = self
                .subjects
                .remove(old)
                .ok_or("remap subject disappeared")?;
            self.subjects.insert(new.clone(), subject);
            if let Some(frames) = self.lens_frames.remove(old) {
                self.lens_frames.insert(new.clone(), frames);
            }
            if let Some(route) = self.input_routes.remove(old) {
                self.input_routes.insert(new.clone(), route);
            }
            if let Some(label) = self.lens_labels.remove(old) {
                self.lens_labels.insert(new.clone(), label);
            }
        }
        self.transfer_drafts = drafts;
        self.transfer_draft_keys = Arc::new(draft_keys);
        Ok(())
    }

    fn reconcile_subject_mounts(&mut self) -> Result<(), String> {
        let mut defaults = BTreeMap::new();
        let mut live_tabs = BTreeSet::new();
        let mut insertions = BTreeMap::new();
        for (owner, set) in self.composition.view_sets() {
            live_tabs.insert(set.id);
            let surface = self
                .composition
                .surfaces()
                .get(owner)
                .ok_or("subject tab has no Surface owner")?;
            let saved = surface.admitted_saved_tab(set.id);
            if let Some(tab) = saved {
                self.template_ids
                    .insert(set.id, tab.saved_set_id().to_owned());
            }
            insertions.insert(
                set.id,
                saved
                    .map(|tab| tab.binding().clone())
                    .unwrap_or_else(|| surface.binding().clone()),
            );
            set.root
                .as_ref()
                .map(|root| root.tile_ids())
                .unwrap_or_default()
                .into_iter()
                .map(|id| {
                    let tile = set.tiles.get(&id).ok_or("subject layout mount absent")?;
                    let mount = self
                        .composition
                        .mount(&tile.instance_key)
                        .ok_or("subject mount has no exact binding owner")?;
                    defaults.insert(
                        tile.instance_key.clone(),
                        UiCompositionMountSubject {
                            binding: mount.binding().clone(),
                            view_ref: mount.view_ref().into(),
                            required_input: None,
                            attachment: super::attachment::SelectionAttachment::FollowViewSet {
                                view_set_id: set.id,
                            },
                        },
                    );
                    Ok(())
                })
                .collect::<Result<Vec<_>, String>>()?;
        }
        // Shared mounts have no implicit Surface selection. Only a prior
        // exact journalled subject survives, and never across rebinding.
        for (instance, metadata) in &self.subjects {
            if matches!(
                instance.mount_owner(),
                Some(crate::ids::UiViewMountOwner::Shared { .. })
            ) && self.composition.mount(instance).is_some_and(|mount| {
                mount.binding() == &metadata.binding && mount.view_ref() == metadata.view_ref
            }) {
                defaults.insert(instance.clone(), metadata.clone());
            }
        }
        defaults.iter_mut().for_each(|(instance, metadata)| {
            if let Some(previous) = self.subjects.get(instance).filter(|previous| {
                previous.binding == metadata.binding && previous.view_ref == metadata.view_ref
            }) {
                *metadata = previous.clone();
            }
        });
        self.subjects = defaults;
        self.lens_frames
            .retain(|instance, _| self.composition.mount(instance).is_some());
        self.input_routes
            .retain(|instance, _| self.composition.mount(instance).is_some());
        self.lens_labels
            .retain(|instance, _| self.composition.mount(instance).is_some());
        self.work_selections.retain(|id, _| live_tabs.contains(id));
        self.template_ids.retain(|id, _| live_tabs.contains(id));
        self.insertion_bindings
            .retain(|id, _| live_tabs.contains(id));
        let draft_scope_live = |key: &super::model::InputBufferKey| {
            let Some(mount) = self.composition.mount(&key.view_instance_key) else {
                return false;
            };
            (key.view_ref == mount.view_ref()
                && key.binding_scope
                    == super::model::InputBufferKey::binding_scope(mount.binding()))
                || self
                    .lens_frames
                    .get(&key.view_instance_key)
                    .is_some_and(|frames| {
                        frames.iter().any(|frame| {
                            key.view_ref == frame.view_ref
                                && key.binding_scope
                                    == super::model::InputBufferKey::binding_scope(&frame.binding)
                        })
                    })
        };
        let live_draft_keys = self
            .transfer_draft_keys
            .iter()
            .filter(|key| draft_scope_live(key))
            .cloned()
            .collect::<BTreeSet<_>>();
        self.transfer_drafts
            .retain(|key, _| live_draft_keys.contains(key));
        self.transfer_draft_keys = Arc::new(live_draft_keys);
        for (id, binding) in insertions {
            self.insertion_bindings.entry(id).or_insert(binding);
        }
        Ok(())
    }

    /// Saved order/relationships are interpreted exactly once at admission,
    /// before any later edits. They must never be reapplied to a changed tree.
    fn hydrate_admitted_saved_subjects(
        &mut self,
        owner: &crate::ids::SurfaceInstanceId,
    ) -> Result<(), String> {
        let surface = self
            .composition
            .surfaces()
            .get(owner)
            .ok_or("saved subject Surface absent")?;
        let collection = surface
            .view_sets()
            .iter()
            .filter_map(|set| {
                surface
                    .admitted_saved_tab(set.id)
                    .map(|tab| (tab.saved_set_id().to_owned(), set.id))
            })
            .collect::<BTreeMap<_, _>>();
        for set in surface.view_sets() {
            if let Some(tab) = surface.admitted_saved_tab(set.id) {
                let ordered = set
                    .root
                    .as_ref()
                    .map(|root| root.tile_ids())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|id| {
                        let tile = set
                            .tiles
                            .get(&id)
                            .ok_or("saved subject layout mount absent")?;
                        Ok((tile.instance_key.clone(), tile.view.view_ref.clone()))
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                let descriptor = self
                    .descriptors
                    .get(&tab.binding().binding_attachment_id)
                    .filter(|descriptor| &coordinate(descriptor) == tab.binding())
                    .ok_or("saved subject descriptor differs from admitted tab")?;
                let surface_value = descriptor
                    .presentation
                    .surface_value()
                    .map_err(|error| error.to_string())?
                    .ok_or("saved tab descriptor is not a Surface")?;
                let views = super::content::views_from_surface(Some(&surface_value));
                let template = tab.template();
                let (values, unresolved, work_selection) = if let Some(particular) =
                    tab.particular()
                {
                    let work = particular.resolved.resolved_selection_work.as_ref();
                    (super::composition_subjects::particular_subject_values(&template, work), false,
                        work.map(|work| serde_json::json!({"thread":work.thread,"chain_root":work.chain_root})))
                } else {
                    let reusable = tab.reusable().ok_or("saved tab body absent")?;
                    (reusable.fresh_subjects.clone(), true, None)
                };
                let plan = super::composition_subjects::project_saved_subjects(
                    &template,
                    &values,
                    unresolved,
                    &collection,
                    set.id,
                    &ordered,
                    &views,
                    descriptor.binding_request_bounds.max_request_bytes,
                )?;
                for (instance, subject) in plan.mounts {
                    let metadata = self
                        .subjects
                        .get_mut(&instance)
                        .ok_or("saved subject mount absent")?;
                    metadata.required_input = subject.required_input;
                    metadata.attachment = subject.attachment;
                }
                if let Some(selection) = work_selection {
                    self.work_selections.insert(set.id, selection);
                }
            }
        }
        Ok(())
    }

    fn advance_revision(&mut self) -> Result<(), String> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or("composition revision exhausted")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::presentation_initialization::UiPresentationInitializationRecord;

    #[test]
    fn repeated_exact_frame_scope_maps_once_but_conflicting_fresh_binding_refuses() {
        let source = UiBindingAttachmentCoordinate {
            binding_attachment_id: "source-binding".into(),
            binding_generation: 1,
            binding_digest: "source-digest".into(),
        };
        let destination = UiBindingAttachmentCoordinate {
            binding_attachment_id: "destination-binding".into(),
            binding_generation: 2,
            binding_digest: "destination-digest".into(),
        };
        let conflicting = UiBindingAttachmentCoordinate {
            binding_attachment_id: "other-destination-binding".into(),
            binding_generation: 2,
            binding_digest: "other-digest".into(),
        };
        let instance = RyeOsViewInstanceKey::surface_tile(
            &crate::ids::PresentationId::try_new("source-presentation").unwrap(),
            &crate::ids::SurfaceInstanceId::try_new("source-surface").unwrap(),
            crate::ids::TileId::new(1),
        );
        let key = super::super::model::InputBufferKey::from_binding_coordinate(
            instance,
            "view:test/one",
            "input",
            &source,
        );
        assert_eq!(
            mapped_transfer_draft_binding(
                &key,
                [
                    ("view:test/one", &source, &destination),
                    ("view:test/one", &source, &destination),
                ],
            ),
            Ok(&destination)
        );
        assert!(
            mapped_transfer_draft_binding(
                &key,
                [
                    ("view:test/one", &source, &destination),
                    ("view:test/one", &source, &conflicting),
                ],
            )
            .is_err()
        );
    }

    fn initial() -> UiPresentationInitialization {
        let session = crate::ui::reducer::test_support::session();
        let membership = &session.presentations[0];
        let mut attachments = session.binding_attachments;
        attachments.sort_by(|a, b| a.binding_attachment_id.cmp(&b.binding_attachment_id));
        UiPresentationInitialization {
            schema: 2,
            selection: super::super::presentation_initialization::UiPresentationInitialSelection::ConfiguredDefaults,
            presentation_id: membership.presentation_id.clone(),
            creation_request_id: "22222222-2222-4222-8222-222222222222".into(),
            root_binding: membership.root_binding.clone(),
            next_mount_id: membership.next_mount_id,
            surfaces: membership.surfaces.clone(),
            binding_attachments: attachments,
        }
    }
    fn event(initial: &UiPresentationInitialization) -> SeatEvent {
        SeatEvent {
            producer_engine_seq: None,
            seq: 4,
            kind: SeatEventKind::PresentationInitialized(UiPresentationInitializationRecord {
                operation_id: initial.creation_request_id.clone(),
                initialization: initial.clone(),
            }),
        }
    }

    #[test]
    fn cross_surface_move_refuses_unresolved_current_follower_before_prepare() {
        use super::super::composition_operation::{
            CompositionMutation, CompositionOwner, CompositionPlacement, CompositionTileTarget,
            UiCompositionRequest,
        };
        let initial = initial();
        let replay = UiCompositionReplay::begin(&initial, &event(&initial)).unwrap();
        let source = replay.composition.active_surface().unwrap();
        let set = source.active_view_set().unwrap();
        let tile = *set.tiles.keys().next().unwrap();
        let old = set.tiles.get(&tile).unwrap().instance_key.clone();
        assert!(matches!(
            &replay.subjects.get(&old).unwrap().attachment,
            super::super::attachment::SelectionAttachment::FollowViewSet { .. }
        ));
        let request = UiCompositionRequest {
            schema: 1,
            presentation_id: replay.composition.id().clone(),
            operation_id: "33333333-3333-4333-8333-333333333333".into(),
            expected_revision: replay.revision,
            expected_next_mount_id: replay.next_mount_id,
            owner: CompositionOwner::Surface {
                surface_instance_id: source.id().clone(),
                binding: source.binding().clone(),
            },
            mutation: CompositionMutation::MoveViewAcrossSurfaces {
                source: CompositionTileTarget {
                    surface_instance_id: source.id().clone(),
                    view_set_id: set.id,
                    tile_id: tile,
                },
                destination: CompositionTileTarget {
                    surface_instance_id: crate::ids::SurfaceInstanceId::try_new(
                        "44444444-4444-4444-8444-444444444444",
                    )
                    .unwrap(),
                    view_set_id: crate::ids::ViewSetId::new(2),
                    tile_id: tile,
                },
                destination_binding: source.binding().clone(),
                placement: CompositionPlacement::First,
            },
        };
        assert!(
            replay
                .stage_delta(&request, 65536)
                .err()
                .expect("follower refuses staging")
                .contains("unresolved current")
        );
        assert!(replay.completed_mount_remaps.is_empty());
    }

    #[test]
    fn completed_cross_surface_move_replays_same_exact_mount_and_context_from_cold_prefix() {
        use super::super::admission_operation::UiPreparedPresentationOperationBody;
        use super::super::attachment::SelectionAttachment;
        use super::super::composition_identity::UiSurfaceMountManifest;
        use super::super::composition_operation::{
            CompositionMutation, CompositionOwner, CompositionPlacement, CompositionTileTarget,
            UiCompositionRequest,
        };
        use sha2::{Digest, Sha256};
        let mut initial = initial();
        let first = initial.surfaces[0].clone();
        let second_id =
            crate::ids::SurfaceInstanceId::try_new("44444444-4444-4444-8444-444444444444").unwrap();
        let seed = first.mount_manifest.tabs[0].seed.clone();
        let manifest = UiSurfaceMountManifest::allocate(
            first.mount_manifest.presentation_id.clone(),
            second_id.clone(),
            initial.next_mount_id,
            vec![(seed.clone(), first.binding.clone())],
            Some(&seed.id),
        )
        .unwrap();
        initial.next_mount_id = manifest.next_mount_id;
        initial.surfaces.push(UiSurfaceMembership {
            surface_instance_id: second_id.as_str().to_owned(),
            ordinal: first.ordinal + 1,
            title: "Second".into(),
            binding: first.binding.clone(),
            mount_manifest: manifest,
            saved_surface: None,
        });
        let surface_descriptor = initial
            .binding_attachments
            .iter_mut()
            .find(|descriptor| {
                descriptor.binding_attachment_id == first.binding.binding_attachment_id
            })
            .unwrap();
        let crate::presentation::UiCompositionDefinition::Surface { surface } =
            &mut surface_descriptor.presentation.definition
        else {
            panic!("fixture needs the exact admitted Surface descriptor")
        };
        surface.tiling.mode = crate::surface::TilingModeSpec::SingleLens;
        let mut history = vec![event(&initial)];
        let mut replay = UiCompositionReplay::begin(&initial, &history[0]).unwrap();
        let source_id = first.mount_manifest.surface_instance_id.clone();
        let source_set = replay.composition.surfaces()[&source_id]
            .active_view_set()
            .unwrap();
        let source_set_id = source_set.id;
        let tile_id = source_set.focused_tile;
        let old = source_set.tiles[&tile_id].instance_key.clone();
        let view_ref = source_set.tiles[&tile_id].view.view_ref.clone();
        let destination_set = replay.composition.surfaces()[&second_id]
            .active_view_set()
            .unwrap();
        let destination_set_id = destination_set.id;
        let destination_tile = destination_set.focused_tile;
        let binding = replay.composition.mount(&old).unwrap().binding().clone();
        let descriptor = &replay.descriptors[&binding.binding_attachment_id];
        let view: super::super::content::ViewBinding =
            serde_json::from_value(descriptor.presentation.views[&view_ref].clone()).unwrap();
        let values = super::super::attachment::selection_dependencies(&view)
            .into_iter()
            .map(|facet| (facet, serde_json::json!({"subject":"fixed"})))
            .collect::<BTreeMap<_, _>>();
        let pinned = SelectionAttachment::Pinned {
            fingerprint: format!("{:x}", Sha256::digest(serde_json::to_vec(&values).unwrap())),
            values,
        };
        let bounds = descriptor.binding_request_bounds;
        let producer = super::super::binding::UiPresentationProducer {
            presentation_id: initial.presentation_id.clone(),
            thread_id: "fixture-seat".into(),
            producer_incarnation: "a".repeat(64),
        };
        let source_owner = CompositionOwner::Surface {
            surface_instance_id: source_id.clone(),
            binding: first.binding.clone(),
        };
        let append = |replay: &mut UiCompositionReplay,
                      history: &mut Vec<SeatEvent>,
                      id: &str,
                      mutation: CompositionMutation| {
            let mut operation = UiPreparedPresentationOperation {
                schema: 2,
                prepared_engine_seq: 0,
                operation_id: id.into(),
                producer: producer.clone(),
                root_binding: initial.root_binding.clone(),
                request_bounds: bounds,
                body: UiPreparedPresentationOperationBody::Composition {
                    request: UiCompositionRequest {
                        schema: 1,
                        presentation_id: replay.composition.id().clone(),
                        operation_id: id.into(),
                        expected_revision: replay.revision,
                        expected_next_mount_id: replay.next_mount_id,
                        owner: source_owner.clone(),
                        mutation,
                    },
                },
            };
            operation.assign_identity().unwrap();
            let completion = replay
                .expected_composition_completion(&operation, bounds.max_request_bytes)
                .unwrap();
            let seq = history.last().unwrap().seq + 1;
            let prepared = SeatEvent {
                producer_engine_seq: None,
                seq,
                kind: SeatEventKind::PresentationOperationPrepared(operation),
            };
            replay.apply_ordered_event(&prepared).unwrap();
            history.push(prepared);
            let completed = SeatEvent {
                producer_engine_seq: None,
                seq: seq + 1,
                kind: SeatEventKind::PresentationOperationCompleted(completion),
            };
            replay.apply_ordered_event(&completed).unwrap();
            history.push(completed);
        };
        append(
            &mut replay,
            &mut history,
            "55555555-5555-4555-8555-555555555555",
            CompositionMutation::SetSubject {
                mount: old.clone(),
                attachment: pinned.clone(),
            },
        );
        let route = SeatEvent {
            producer_engine_seq: None,
            seq: history.last().unwrap().seq + 1,
            kind: SeatEventKind::Facet {
                key: super::super::seat::input_route_facet_key(&old),
                value: serde_json::json!({"thread":"original"}),
            },
        };
        replay.apply_ordered_event(&route).unwrap();
        history.push(route);
        append(
            &mut replay,
            &mut history,
            "77777777-7777-4777-8777-777777777777",
            CompositionMutation::LensPush {
                tab_id: source_set_id,
                tile_id,
                view_ref: view_ref.clone(),
                binding: binding.clone(),
                attachment: pinned.clone(),
                input_route: Some(serde_json::json!({"thread":"child"})),
                label: Some("Child".into()),
            },
        );
        let old_frames = replay.lens_frames[&old].clone();
        let old_label = replay.lens_labels[&old].clone();
        // Both the current lens and its return frame have explicit subjects.
        // The canonical route/frame context must follow the exact instance.
        let old_route = replay.input_routes[&old].clone();
        let source_target = CompositionTileTarget {
            surface_instance_id: source_id,
            view_set_id: source_set_id,
            tile_id,
        };
        let destination_target = CompositionTileTarget {
            surface_instance_id: second_id.clone(),
            view_set_id: destination_set_id,
            tile_id: destination_tile,
        };
        let move_mutation = CompositionMutation::MoveViewAcrossSurfaces {
            source: source_target,
            destination: destination_target,
            destination_binding: first.binding,
            placement: CompositionPlacement::Split {
                target: destination_tile,
                axis: crate::layout::SplitAxis::Horizontal,
                before: false,
                ratio: 0.5,
            },
        };
        let mut operation = UiPreparedPresentationOperation {
            schema: 2,
            prepared_engine_seq: 0,
            operation_id: "66666666-6666-4666-8666-666666666666".into(),
            producer: producer.clone(),
            root_binding: initial.root_binding.clone(),
            request_bounds: bounds,
            body: UiPreparedPresentationOperationBody::Composition {
                request: UiCompositionRequest {
                    schema: 1,
                    presentation_id: replay.composition.id().clone(),
                    operation_id: "66666666-6666-4666-8666-666666666666".into(),
                    expected_revision: replay.revision,
                    expected_next_mount_id: replay.next_mount_id,
                    owner: source_owner.clone(),
                    mutation: move_mutation,
                },
            },
        };
        operation.assign_identity().unwrap();
        let completion = replay
            .expected_composition_completion(&operation, bounds.max_request_bytes)
            .unwrap();
        let operation_id = operation.operation_id.clone();
        let before = replay.clone();
        let prepared = SeatEvent {
            producer_engine_seq: None,
            seq: history.last().unwrap().seq + 1,
            kind: SeatEventKind::PresentationOperationPrepared(operation),
        };
        replay.apply_ordered_event(&prepared).unwrap();
        history.push(prepared);
        assert!(replay.composition.mount(&old).is_some());
        assert_eq!(replay.subjects, before.subjects);
        assert_eq!(replay.input_routes, before.input_routes);
        assert!(replay.completed_mount_remaps.is_empty());
        let crossed_route = SeatEvent {
            producer_engine_seq: None,
            seq: history.last().unwrap().seq + 1,
            kind: SeatEventKind::Facet {
                key: super::super::seat::input_route_facet_key(&old),
                value: serde_json::json!({"thread":"crossed-pending-move"}),
            },
        };
        assert!(replay.apply_ordered_event(&crossed_route).is_err());
        assert_eq!(replay.input_routes, before.input_routes);
        assert_eq!(replay.subjects, before.subjects);
        assert!(replay.composition.mount(&old).is_some());
        let completed = SeatEvent {
            producer_engine_seq: None,
            seq: history.last().unwrap().seq + 1,
            kind: SeatEventKind::PresentationOperationCompleted(completion),
        };
        replay.apply_ordered_event(&completed).unwrap();
        history.push(completed);
        let new = RyeOsViewInstanceKey::surface_tile(replay.composition.id(), &second_id, tile_id);
        assert!(replay.composition.mount(&old).is_none());
        assert_eq!(replay.composition.mount(&new).unwrap().binding(), &binding);
        assert_eq!(replay.subjects[&new].attachment, pinned);
        assert_eq!(replay.input_routes[&new], old_route);
        assert_eq!(replay.lens_frames[&new], old_frames);
        assert_eq!(replay.lens_labels[&new], old_label);
        assert_eq!(replay.completed_mount_remaps[&operation_id][&old], new);
        let cold = UiCompositionReplay::from_acknowledged_prefix(&initial, &history).unwrap();
        assert_eq!(cold.subjects, replay.subjects);
        assert_eq!(cold.input_routes, replay.input_routes);
        assert_eq!(cold.lens_frames, replay.lens_frames);
        assert_eq!(cold.lens_labels, replay.lens_labels);
        assert_eq!(cold.completed_mount_remaps, replay.completed_mount_remaps);
        assert!(cold.composition.mount(&old).is_none());
        assert!(cold.composition.mount(&new).is_some());
    }

    #[test]
    fn retained_freeze_guards_exact_facets_and_empty_set_closure() {
        use super::super::transfer_export::{
            UiTransferExport, UiTransferExportContent, UiTransferExportMode, UiTransferSetExport,
        };
        use crate::ids::{TileId, UiViewMountOwner};
        use ryeos_ui_presentation_contract::transfer::{TransferIdentity, TransferSeat};
        let initial = initial();
        let mut replay = UiCompositionReplay::begin(&initial, &event(&initial)).unwrap();
        let surface = replay.composition.surfaces().values().next().unwrap();
        let surface_id = surface.id().clone();
        let set = surface.active_view_set().unwrap();
        let set_id = set.id;
        let instance = set.tiles.values().next().unwrap().instance_key.clone();
        // Isolate the guard matcher: admission/snapshot validation is tested
        // separately. Empty set closure must rely on its explicit owner.
        let seat = TransferSeat {
            presentation_id: initial.presentation_id.clone(),
            thread_id: "thread".into(),
            chain_root_id: "root".into(),
            producer_incarnation: "ab".repeat(32),
            revision: 0,
        };
        replay.move_freezes.insert(
            "guard-fixture".into(),
            super::super::transfer_freeze::UiMoveFreezeRequest {
                schema: 1,
                operation_id: "guard-fixture".into(),
                transfer: TransferIdentity {
                    operation_id: "transfer-fixture".into(),
                    source: seat.clone(),
                    destination: seat,
                    export_digest: "cd".repeat(32),
                },
                root_binding: initial.root_binding.clone(),
                instances: vec![instance.clone()],
                view_sets: vec![set_id],
                export: UiTransferExport {
                    schema: 1,
                    mode: UiTransferExportMode::Move,
                    content: UiTransferExportContent::ViewSets {
                        active_set: None,
                        sets: vec![UiTransferSetExport {
                            handle: "empty".into(),
                            source_view_set_id: set_id,
                            source_owner: UiViewMountOwner::Surface {
                                presentation_id: replay.composition.id().clone(),
                                surface_instance_id: surface_id.clone(),
                            },
                            binding: initial.root_binding.clone(),
                            seed: crate::surface::view_sets::ViewSetSeedSpec {
                                schema: crate::surface::view_sets::VIEW_SET_COMPOSITION_SCHEMA,
                                id: "empty".into(),
                                title: "Empty".into(),
                                root: None,
                            },
                            focused_view: None,
                            mounts: vec![],
                        }],
                    },
                },
            },
        );
        let facet = |key| SeatEvent {
            producer_engine_seq: None,
            seq: 5,
            kind: SeatEventKind::Facet {
                key,
                value: serde_json::Value::Null,
            },
        };
        assert!(
            replay
                .clone()
                .apply_ordered_event_owned(&facet(super::super::seat::input_route_facet_key(
                    &instance
                )))
                .is_err()
        );
        assert!(
            replay
                .clone()
                .apply_ordered_event_owned(&facet(
                    super::super::seat::selection_storage_key(set_id, "selection.work").unwrap()
                ))
                .is_err()
        );
        let other = RyeOsViewInstanceKey::surface_tile(
            replay.composition.id(),
            &surface_id,
            TileId::new(u64::MAX),
        );
        assert!(
            replay
                .clone()
                .apply_ordered_event_owned(&facet(super::super::seat::input_route_facet_key(
                    &other
                )))
                .is_ok()
        );
        assert!(
            replay
                .clone()
                .apply_ordered_event_owned(&facet("unrelated.preference".into()))
                .is_ok()
        );
        let closure = SeatEvent {
            producer_engine_seq: None,
            seq: 5,
            kind: SeatEventKind::SurfaceClosed(super::super::seat::UiSurfaceClosure {
                presentation_id: initial.presentation_id.clone(),
                surface_instance_id: surface_id.as_str().into(),
                binding: initial.root_binding.clone(),
            }),
        };
        assert!(replay.validate_move_freeze_event(&closure).is_err());
        assert_eq!(replay.revision, 0);
    }

    #[test]
    fn routed_replacement_publishes_exact_override_only_at_completion() {
        use super::super::composition_operation::{
            CompositionMutation, CompositionOwner, UiCompositionRequest,
            UiCompositionRouteAssignment,
        };
        let initial = initial();
        let mut replay = UiCompositionReplay::begin(&initial, &event(&initial)).unwrap();
        let set = replay.composition.active_view_set().unwrap();
        let tab_id = set.id;
        let tile_id = set.focused_tile;
        let instance = set.tiles[&tile_id].instance_key.clone();
        let view_ref = set.tiles[&tile_id].view.view_ref.clone();
        let surface = replay.composition.active_surface().unwrap();
        let owner = CompositionOwner::Surface {
            surface_instance_id: surface.id().clone(),
            binding: surface.binding().clone(),
        };
        let binding = replay
            .composition
            .mount(&instance)
            .unwrap()
            .binding()
            .clone();
        let bounds = replay.descriptors[&binding.binding_attachment_id].binding_request_bounds;
        for (index, route) in [Some(serde_json::json!({"thread":"destination"})), None]
            .into_iter()
            .enumerate()
        {
            let id = format!("33333333-3333-4333-8333-{:012}", index + 1);
            let mut operation = UiPreparedPresentationOperation {
                schema: 2,
                prepared_engine_seq: 0,
                operation_id: id.clone(),
                producer: super::super::binding::UiPresentationProducer {
                    presentation_id: initial.presentation_id.clone(), thread_id:"seat".into(), producer_incarnation:"ab".repeat(32),
                },
                root_binding: initial.root_binding.clone(), request_bounds: bounds,
                body: super::super::admission_operation::UiPreparedPresentationOperationBody::Composition {
                    request: UiCompositionRequest {
                        schema:1, presentation_id:replay.composition.id().clone(), operation_id:id,
                        expected_revision:replay.revision, expected_next_mount_id:replay.next_mount_id, owner:owner.clone(),
                        mutation:CompositionMutation::ReplaceView {
                            tab_id, tile_id, view_ref:view_ref.clone(), binding:binding.clone(),
                            subject:crate::surface::view_sets::SavedViewSelectionSource::FollowOwnSet,
                            route_assignment:UiCompositionRouteAssignment::Override { input_route:route.clone() },
                        },
                    },
                },
            };
            operation.assign_identity().unwrap();
            let before = replay.input_routes.clone();
            let completion = replay
                .expected_composition_completion(&operation, bounds.max_request_bytes)
                .unwrap();
            assert_eq!(replay.input_routes, before);
            let seq = 5 + index as u64 * 2;
            replay
                .apply_ordered_event(&SeatEvent {
                    producer_engine_seq: None,
                    seq,
                    kind: SeatEventKind::PresentationOperationPrepared(operation),
                })
                .unwrap();
            assert_eq!(replay.input_routes, before);
            assert!(
                replay
                    .apply_ordered_event(&SeatEvent {
                        producer_engine_seq: None,
                        seq: seq + 1,
                        kind: SeatEventKind::Facet {
                            key: format!(
                                "{}\u{1f}{}",
                                super::super::seat::KEY_INPUT_ROUTE,
                                instance
                            ),
                            value: serde_json::json!({"thread":"stale-replacement-route"}),
                        },
                    })
                    .is_err()
            );
            assert_eq!(replay.input_routes, before);
            let mut contradicted = completion.clone();
            contradicted.result["next_mount_id"] = serde_json::json!(replay.next_mount_id + 1);
            let frames_before = replay.lens_frames.clone();
            let labels_before = replay.lens_labels.clone();
            assert!(
                replay
                    .apply_ordered_event(&SeatEvent {
                        producer_engine_seq: None,
                        seq: seq + 1,
                        kind: SeatEventKind::PresentationOperationCompleted(contradicted),
                    })
                    .is_err()
            );
            assert_eq!(replay.input_routes, before);
            assert_eq!(replay.lens_frames, frames_before);
            assert_eq!(replay.lens_labels, labels_before);
            replay
                .apply_ordered_event(&SeatEvent {
                    producer_engine_seq: None,
                    seq: seq + 1,
                    kind: SeatEventKind::PresentationOperationCompleted(completion),
                })
                .unwrap();
            assert_eq!(replay.input_routes[&instance].1, route);
            assert_eq!(
                replay.composition.active_view_set().unwrap().tiles[&tile_id].instance_key,
                instance
            );
        }
    }

    #[test]
    fn retained_lens_routes_and_return_frames_publish_only_after_completion() {
        use super::super::admission_operation::UiPreparedPresentationOperationBody;
        use super::super::composition_operation::{
            CompositionMutation, CompositionOwner, UiCompositionRequest,
        };
        let mut initial = initial();
        let surface_binding = initial.surfaces[0].binding.clone();
        let descriptor = initial
            .binding_attachments
            .iter_mut()
            .find(|descriptor| coordinate(descriptor) == surface_binding)
            .unwrap();
        let crate::presentation::UiCompositionDefinition::Surface { surface } =
            &mut descriptor.presentation.definition
        else {
            panic!("actual Surface fixture required")
        };
        surface.tiling.mode = crate::surface::TilingModeSpec::SingleLens;
        let mut replay = UiCompositionReplay::begin(&initial, &event(&initial)).unwrap();
        let set = replay.composition.active_view_set().unwrap();
        let tab_id = set.id;
        let tile_id = set.focused_tile;
        let instance = set.tiles[&tile_id].instance_key.clone();
        let view_ref = set.tiles[&tile_id].view.view_ref.clone();
        let surface = replay.composition.active_surface().unwrap();
        let owner = CompositionOwner::Surface {
            surface_instance_id: surface.id().clone(),
            binding: surface.binding().clone(),
        };
        let binding = replay
            .composition
            .mount(&instance)
            .unwrap()
            .binding()
            .clone();
        let bounds = replay.descriptors[&binding.binding_attachment_id].binding_request_bounds;
        replay
            .apply_ordered_event(&SeatEvent {
                producer_engine_seq: None,
                seq: 5,
                kind: SeatEventKind::Facet {
                    key: super::super::seat::input_route_facet_key(&instance),
                    value: serde_json::json!({"thread":"original"}),
                },
            })
            .unwrap();
        assert_eq!(
            replay.input_routes[&instance].1.as_ref().unwrap()["thread"],
            "original"
        );
        let mut seq = 6;
        for (id, mutation) in [
            (
                "33333333-3333-4333-8333-333333333333",
                CompositionMutation::LensPush {
                    tab_id,
                    tile_id,
                    view_ref,
                    binding,
                    attachment: super::super::attachment::SelectionAttachment::FollowViewSet {
                        view_set_id: tab_id,
                    },
                    input_route: Some(serde_json::json!({"thread":"child"})),
                    label: Some("Child".into()),
                },
            ),
            (
                "44444444-4444-4444-8444-444444444444",
                CompositionMutation::LensPop { tab_id, tile_id },
            ),
        ] {
            let mut operation = UiPreparedPresentationOperation {
                schema: 2,
                prepared_engine_seq: 0,
                operation_id: id.into(),
                producer: super::super::binding::UiPresentationProducer {
                    presentation_id: initial.presentation_id.clone(),
                    thread_id: "seat".into(),
                    producer_incarnation: "ab".repeat(32),
                },
                root_binding: initial.root_binding.clone(),
                request_bounds: bounds,
                body: UiPreparedPresentationOperationBody::Composition {
                    request: UiCompositionRequest {
                        schema: 1,
                        presentation_id: replay.composition.id().clone(),
                        operation_id: id.into(),
                        expected_revision: replay.revision,
                        expected_next_mount_id: replay.next_mount_id,
                        owner: owner.clone(),
                        mutation,
                    },
                },
            };
            operation.assign_identity().unwrap();
            let before = replay.input_routes.clone();
            let completion = replay
                .expected_composition_completion(&operation, bounds.max_request_bytes)
                .unwrap();
            assert_eq!(replay.input_routes, before);
            replay
                .apply_ordered_event(&SeatEvent {
                    producer_engine_seq: None,
                    seq,
                    kind: SeatEventKind::PresentationOperationPrepared(operation),
                })
                .unwrap();
            assert_eq!(replay.input_routes, before);
            seq += 1;
            assert!(
                replay
                    .apply_ordered_event(&SeatEvent {
                        producer_engine_seq: None,
                        seq,
                        kind: SeatEventKind::Facet {
                            key: super::super::seat::input_route_facet_key(&instance),
                            value: serde_json::json!({"thread":"stale-writer"}),
                        },
                    })
                    .is_err()
            );
            assert_eq!(replay.input_routes, before);
            replay
                .apply_ordered_event(&SeatEvent {
                    producer_engine_seq: None,
                    seq,
                    kind: SeatEventKind::PresentationOperationCompleted(completion),
                })
                .unwrap();
            seq += 1;
        }
        assert_eq!(
            replay.composition.active_view_set().unwrap().tiles[&tile_id].instance_key,
            instance
        );
        assert!(replay.lens_frames[&instance].is_empty());
        assert_eq!(
            replay.input_routes[&instance].1.as_ref().unwrap()["thread"],
            "original"
        );
        assert_eq!(replay.lens_labels[&instance], None);
    }

    #[test]
    fn pinned_insert_has_no_mount_or_subject_before_exact_completion() {
        use super::super::admission_operation::UiPreparedPresentationOperationBody;
        use super::super::composition_operation::{
            CompositionMutation, CompositionOwner, CompositionPlacement, UiCompositionRequest,
        };
        let mut initial = initial();
        let surface = initial.surfaces[0].clone();
        let mut preliminary = UiCompositionReplay::begin(&initial, &event(&initial)).unwrap();
        let set = preliminary.composition.active_view_set().unwrap();
        let target = set.focused_tile;
        let tab_id = set.id;
        let view_ref = set.tiles[&target].view.view_ref.clone();
        let descriptor = initial
            .binding_attachments
            .iter_mut()
            .find(|descriptor| {
                descriptor.binding_attachment_id == surface.binding.binding_attachment_id
            })
            .unwrap();
        descriptor.presentation.views.insert(
            view_ref.clone(),
            serde_json::json!({"facet":"selection.work"}),
        );
        let bounds = descriptor.binding_request_bounds;
        preliminary = UiCompositionReplay::begin(&initial, &event(&initial)).unwrap();
        let old_tiles = preliminary
            .composition
            .active_view_set()
            .unwrap()
            .tile_ids();
        let values = BTreeMap::from([(
            "selection.work".into(),
            serde_json::json!({"thread_id":"T-pinned"}),
        )]);
        use sha2::{Digest, Sha256};
        let pin = super::super::attachment::SelectionAttachment::Pinned {
            fingerprint: format!("{:x}", Sha256::digest(serde_json::to_vec(&values).unwrap())),
            values,
        };
        let inserted_binding = surface.binding.clone();
        let mut operation = UiPreparedPresentationOperation {
            schema: 2,
            prepared_engine_seq: 0,
            operation_id: "33333333-3333-4333-8333-333333333333".into(),
            producer: super::super::binding::UiPresentationProducer {
                presentation_id: initial.presentation_id.clone(),
                thread_id: "T-test".into(),
                producer_incarnation: "a".repeat(64),
            },
            root_binding: initial.root_binding.clone(),
            request_bounds: bounds,
            body: UiPreparedPresentationOperationBody::Composition {
                request: UiCompositionRequest {
                    schema: 1,
                    presentation_id: preliminary.composition.id().clone(),
                    operation_id: "33333333-3333-4333-8333-333333333333".into(),
                    expected_revision: 0,
                    expected_next_mount_id: preliminary.next_mount_id,
                    owner: CompositionOwner::Surface {
                        surface_instance_id: crate::ids::SurfaceInstanceId::try_new(
                            surface.surface_instance_id,
                        )
                        .unwrap(),
                        binding: surface.binding,
                    },
                    mutation: CompositionMutation::InsertPinnedView {
                        tab_id,
                        placement: CompositionPlacement::Split {
                            target,
                            axis: crate::layout::SplitAxis::Horizontal,
                            before: false,
                            ratio: 0.5,
                        },
                        view_ref,
                        binding: inserted_binding,
                        attachment: pin.clone(),
                        route_assignment: super::super::composition_operation::UiCompositionRouteAssignment::Authored,
                    },
                },
            },
        };
        operation.assign_identity().unwrap();
        let completion = preliminary
            .expected_composition_completion(&operation, bounds.max_request_bytes)
            .unwrap();
        preliminary
            .apply_ordered_event(&SeatEvent {
                producer_engine_seq: None,
                seq: 5,
                kind: SeatEventKind::PresentationOperationPrepared(operation),
            })
            .unwrap();
        assert_eq!(
            preliminary
                .composition
                .active_view_set()
                .unwrap()
                .tile_ids(),
            old_tiles
        );
        preliminary
            .apply_ordered_event(&SeatEvent {
                producer_engine_seq: None,
                seq: 6,
                kind: SeatEventKind::PresentationOperationCompleted(completion),
            })
            .unwrap();
        let set = preliminary.composition.active_view_set().unwrap();
        let created = set
            .tiles
            .iter()
            .find(|(id, _)| !old_tiles.contains(id))
            .unwrap()
            .1;
        assert_eq!(preliminary.subjects[&created.instance_key].attachment, pin);
        assert_eq!(set.tile_ids().len(), old_tiles.len() + 1);
    }

    #[test]
    fn journalled_shared_subject_applies_only_on_completion_and_survives_reconcile() {
        use super::super::admission_operation::UiPreparedPresentationOperationBody;
        use super::super::composition_operation::{
            CompositionMutation, CompositionOwner, UiCompositionRequest,
        };
        let initial = initial();
        let mut replay = UiCompositionReplay::begin(&initial, &event(&initial)).unwrap();
        let instance = replay
            .composition
            .dialogs()
            .values()
            .next()
            .unwrap()
            .view()
            .instance_key()
            .clone();
        let mounted = replay.composition.mount(&instance).unwrap();
        let descriptor = &replay.descriptors[&mounted.binding().binding_attachment_id];
        let binding: super::super::content::ViewBinding =
            serde_json::from_value(descriptor.presentation.views[mounted.view_ref()].clone())
                .unwrap();
        let values = super::super::attachment::selection_dependencies(&binding)
            .into_iter()
            .map(|facet| (facet, serde_json::json!({"subject":"observed"})))
            .collect::<BTreeMap<_, _>>();
        use sha2::{Digest, Sha256};
        let attachment = super::super::attachment::SelectionAttachment::Pinned {
            fingerprint: format!("{:x}", Sha256::digest(serde_json::to_vec(&values).unwrap())),
            values,
        };
        let mut operation = UiPreparedPresentationOperation {
            schema: 2,
            prepared_engine_seq: 0,
            operation_id: "33333333-3333-4333-8333-333333333333".into(),
            producer: super::super::binding::UiPresentationProducer {
                presentation_id: initial.presentation_id.clone(),
                thread_id: "T-test".into(),
                producer_incarnation: "a".repeat(64),
            },
            root_binding: initial.root_binding.clone(),
            request_bounds: descriptor.binding_request_bounds,
            body: UiPreparedPresentationOperationBody::Composition {
                request: UiCompositionRequest {
                    schema: 1,
                    presentation_id: replay.composition.id().clone(),
                    operation_id: "33333333-3333-4333-8333-333333333333".into(),
                    expected_revision: 0,
                    expected_next_mount_id: replay.next_mount_id,
                    owner: CompositionOwner::Shared {
                        binding: initial.root_binding.clone(),
                    },
                    mutation: CompositionMutation::SetSubject {
                        mount: instance.clone(),
                        attachment: attachment.clone(),
                    },
                },
            },
        };
        operation.assign_identity().unwrap();
        let completion = replay
            .expected_composition_completion(&operation, operation.request_bounds.max_request_bytes)
            .unwrap();
        replay
            .apply_ordered_event(&SeatEvent {
                producer_engine_seq: None,
                seq: 5,
                kind: SeatEventKind::PresentationOperationPrepared(operation),
            })
            .unwrap();
        assert!(!replay.subjects.contains_key(&instance));
        replay
            .apply_ordered_event(&SeatEvent {
                producer_engine_seq: None,
                seq: 6,
                kind: SeatEventKind::PresentationOperationCompleted(completion),
            })
            .unwrap();
        assert_eq!(replay.subjects[&instance].attachment, attachment);
        replay.reconcile_subject_mounts().unwrap();
        assert_eq!(replay.subjects[&instance].attachment, attachment);
    }

    #[test]
    fn particular_single_tab_replays_exact_manifest_and_work_without_synthetic_facets() {
        use super::super::admission_operation::{
            UiCompletedPresentationOperation, UiPreparedAdmissionOperation,
            UiPreparedPresentationOperationBody, UiPresentationOperationKind,
        };
        use super::super::binding::{
            UiBindingCoordinate, UiBindingPayload, UiBindingRequest, UiPresentationProducer,
        };
        let initial = initial();
        let mut replay = UiCompositionReplay::begin(&initial, &event(&initial)).unwrap();
        let origin = replay
            .composition
            .view_sets()
            .next()
            .unwrap()
            .1
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        let mount = replay.composition.mount(&origin).unwrap();
        let binding = mount.binding().clone();
        let view_ref = mount.view_ref().to_owned();
        let descriptor = replay.binding_descriptors()[&binding.binding_attachment_id].clone();
        let operation_id = "55555555-5555-4555-8555-555555555555";
        let mut prepared = UiPreparedPresentationOperation {
            schema: 2,
            prepared_engine_seq: 0,
            operation_id: operation_id.into(),
            producer: UiPresentationProducer {
                presentation_id: initial.presentation_id.clone(),
                thread_id: "fixture-seat".into(),
                producer_incarnation: "a".repeat(64),
            },
            root_binding: initial.root_binding.clone(),
            request_bounds: descriptor.binding_request_bounds,
            body: UiPreparedPresentationOperationBody::Admission(UiPreparedAdmissionOperation {
                purpose: UiPresentationOperationKind::ResumeParticularViewSet,
                surface_open_target: None,
                particular_selection: Some(
                    crate::ui::admission_operation::UiParticularAdmissionSelection::ViewSet {
                        id: "particular".into(),
                        expected_revision: 1,
                        surface_ref: descriptor.root_ref.clone(),
                    },
                ),
                invocation_origin: origin,
                request: UiBindingRequest {
                    binding_attachment_id: binding.binding_attachment_id.clone(),
                    binding_generation: binding.binding_generation,
                    binding_digest: binding.binding_digest.clone(),
                    coordinate: UiBindingCoordinate::Affordance {
                        view_ref: view_ref.clone(),
                        affordance_id: "resume".into(),
                    },
                    payload: UiBindingPayload::Selection {
                        record: serde_json::json!({"creation_request_id":operation_id}),
                    },
                },
                success_notice: None,
            }),
        };
        prepared.assign_identity().unwrap();
        let operation_id = prepared.operation_id.clone();
        let resolved:crate::surface::view_sets::ResolvedParticularViewSet = serde_json::from_value(serde_json::json!({
            "id":"particular","name":"Particular","composition":{"schema":2,"id":"particular","title":"Particular","root":{"type":"group","views":[view_ref],"active":0}},
            "relationships":[{"mount":{"kind":"tile","index":0},"source":{"mode":"follow_own_set"}}],
            "resolved_selection_work":{"thread":"T-head","chain_root":"T-root"}
        })).unwrap();
        let surface_id =
            crate::ids::SurfaceInstanceId::try_new("66666666-6666-4666-8666-666666666666").unwrap();
        let manifest = super::super::composition_identity::UiSurfaceMountManifest::allocate(
            replay.composition.id().clone(),
            surface_id.clone(),
            replay.next_mount_id,
            vec![(resolved.composition.clone(), binding.clone())],
            Some("particular"),
        )
        .unwrap();
        let membership = UiSurfaceMembership {
            surface_instance_id: surface_id.as_str().to_owned(),
            ordinal: 2,
            title: "Particular".into(),
            binding: binding.clone(),
            mount_manifest: manifest.clone(),
            saved_surface: None,
        };
        let completion = UiCompletedPresentationOperation {
            operation_id: operation_id.clone(),
            result: serde_json::json!({
                "particular_selection":{"kind":"view_set","id":"particular","expected_revision":1,"surface_ref":descriptor.root_ref},
                "particular_view_set":resolved,"ui_transition":{"kind":"resume_particular_view_set","presentation_id":initial.presentation_id,
                    "creation_request_id":operation_id,"surface":membership,"attachment":descriptor}
            }),
        };
        replay
            .apply_ordered_event(&SeatEvent {
                producer_engine_seq: None,
                seq: 5,
                kind: SeatEventKind::PresentationOperationPrepared(prepared.clone()),
            })
            .unwrap();
        let before = replay.clone();
        let mut contradictory = completion.clone();
        contradictory.result["particular_view_set"]["composition"]["title"] =
            serde_json::json!("Not admitted");
        assert!(
            replay
                .apply_ordered_event(&SeatEvent {
                    producer_engine_seq: None,
                    seq: 6,
                    kind: SeatEventKind::PresentationOperationCompleted(contradictory)
                })
                .is_err()
        );
        assert_eq!(replay.next_mount_id, before.next_mount_id);
        replay
            .apply_ordered_event(&SeatEvent {
                producer_engine_seq: None,
                seq: 6,
                kind: SeatEventKind::PresentationOperationCompleted(completion),
            })
            .unwrap();
        let tab_id = manifest.tabs[0].view_set_id;
        assert_eq!(
            replay.work_selections[&tab_id],
            serde_json::json!({"thread":"T-head","chain_root":"T-root"})
        );
        assert_eq!(replay.insertion_bindings[&tab_id], binding);
        assert!(replay.pending.is_empty());
        assert_eq!(
            replay.admitted_surfaces[surface_id.as_str()].mount_manifest,
            manifest
        );
        assert_eq!(replay.revision, 1);
        let mut wrong_origin = before;
        let mut wrong = prepared;
        if let UiPreparedPresentationOperationBody::Admission(admission) = &mut wrong.body {
            admission.request.coordinate = UiBindingCoordinate::Affordance {
                view_ref: "view:other/sibling".into(),
                affordance_id: "resume".into(),
            };
        }
        // A different operation cannot borrow this binding's sibling View.
        wrong.operation_id = "77777777-7777-4777-8777-777777777777".into();
        wrong_origin.pending.clear();
        if let UiPreparedPresentationOperationBody::Admission(admission) = &mut wrong.body {
            admission.request.payload = UiBindingPayload::Selection {
                record: serde_json::json!({"creation_request_id":wrong.operation_id}),
            };
        }
        assert!(
            wrong_origin
                .apply_ordered_event(&SeatEvent {
                    producer_engine_seq: None,
                    seq: 6,
                    kind: SeatEventKind::PresentationOperationPrepared(wrong)
                })
                .is_err()
        );
    }
    #[test]
    fn closing_internal_follow_target_retains_relation_without_own_set_fallback() {
        use super::super::admission_operation::{
            UiCompletedPresentationOperation, UiPreparedPresentationOperationBody,
        };
        use super::super::composition_operation::{
            CompositionMutation, CompositionOwner, CompositionTabSeed, UiCompositionRequest,
        };
        let mut initial = initial();
        let owner_binding = initial.surfaces[0].binding.binding_attachment_id.clone();
        let view_ref = "view:test/home";
        initial.binding_attachments.iter_mut()
            .find(|descriptor| descriptor.binding_attachment_id == owner_binding)
            .unwrap().presentation.views.insert(view_ref.into(), serde_json::json!({
                "widget":"rows",
                "sources":{"default":{"ref":"service:test/selection","params":{"thread":"@facet:selection.work.thread"}}}
            }));
        let owner = &initial.surfaces[0];
        let descriptor = initial
            .binding_attachments
            .iter()
            .find(|descriptor| {
                descriptor.binding_attachment_id == owner.binding.binding_attachment_id
            })
            .unwrap();
        let tab = |id: &str, source: serde_json::Value| {
            CompositionTabSeed {
            focused_view: None,
            fresh_subjects: BTreeMap::new(),
            followed_sets: BTreeMap::new(),
            template: serde_json::from_value(serde_json::json!({"id":id,"name":id,
                "composition":{"schema":2,"id":id,"title":id,"root":{"type":"group","views":[view_ref],"active":0}},
                "relationships":[{"mount":{"kind":"tile","index":0},"source":source}]
            })).unwrap(), binding:owner.binding.clone(),
        }
        };
        let mut replay = UiCompositionReplay::begin(&initial, &event(&initial)).unwrap();
        let producer = super::super::binding::UiPresentationProducer {
            presentation_id: initial.presentation_id.clone(),
            thread_id: "fixture-seat".into(),
            producer_incarnation: "a".repeat(64),
        };
        let request = UiCompositionRequest {
            schema: 1,
            presentation_id: replay.composition.id().clone(),
            operation_id: "33333333-3333-4333-8333-333333333333".into(),
            expected_revision: 0,
            expected_next_mount_id: replay.next_mount_id,
            owner: CompositionOwner::Surface {
                surface_instance_id: owner.mount_manifest.surface_instance_id.clone(),
                binding: owner.binding.clone(),
            },
            mutation: CompositionMutation::RestoreTabs {
                tabs: vec![
                    tab("target", serde_json::json!({"mode":"follow_own_set"})),
                    tab(
                        "follower",
                        serde_json::json!({"mode":"follow_set","saved_view_set_id":"target"}),
                    ),
                ],
                active_seed_id: "follower".into(),
            },
        };
        let mut prepared = UiPreparedPresentationOperation {
            schema: 2,
            prepared_engine_seq: 0,
            operation_id: request.operation_id.clone(),
            producer: producer.clone(),
            root_binding: initial.root_binding.clone(),
            body: UiPreparedPresentationOperationBody::Composition { request },
            request_bounds: descriptor.binding_request_bounds,
        };
        prepared.assign_identity().unwrap();
        let completed = replay
            .expected_composition_completion(
                &prepared,
                descriptor.binding_request_bounds.max_request_bytes,
            )
            .unwrap();
        replay
            .apply_ordered_event(&SeatEvent {
                producer_engine_seq: None,
                seq: 5,
                kind: SeatEventKind::PresentationOperationPrepared(prepared),
            })
            .unwrap();
        replay
            .apply_ordered_event(&SeatEvent {
                producer_engine_seq: None,
                seq: 6,
                kind: SeatEventKind::PresentationOperationCompleted(completed),
            })
            .unwrap();
        let sets =
            replay.composition.surfaces()[&owner.mount_manifest.surface_instance_id].view_sets();
        let target = sets[0].id;
        let follower = sets[1].id;
        let instance = sets[1].tiles.values().next().unwrap().instance_key.clone();
        let request = UiCompositionRequest {
            schema: 1,
            presentation_id: replay.composition.id().clone(),
            operation_id: "44444444-4444-4444-8444-444444444444".into(),
            expected_revision: replay.revision,
            expected_next_mount_id: replay.next_mount_id,
            owner: CompositionOwner::Surface {
                surface_instance_id: owner.mount_manifest.surface_instance_id.clone(),
                binding: owner.binding.clone(),
            },
            mutation: CompositionMutation::CloseTab { tab_id: target },
        };
        let mut prepared = UiPreparedPresentationOperation {
            schema: 2,
            prepared_engine_seq: 0,
            operation_id: request.operation_id.clone(),
            producer,
            root_binding: initial.root_binding.clone(),
            body: UiPreparedPresentationOperationBody::Composition { request },
            request_bounds: descriptor.binding_request_bounds,
        };
        prepared.assign_identity().unwrap();
        let completed: UiCompletedPresentationOperation = replay
            .expected_composition_completion(
                &prepared,
                descriptor.binding_request_bounds.max_request_bytes,
            )
            .unwrap();
        replay
            .apply_ordered_event(&SeatEvent {
                producer_engine_seq: None,
                seq: 7,
                kind: SeatEventKind::PresentationOperationPrepared(prepared),
            })
            .unwrap();
        replay
            .apply_ordered_event(&SeatEvent {
                producer_engine_seq: None,
                seq: 8,
                kind: SeatEventKind::PresentationOperationCompleted(completed),
            })
            .unwrap();
        assert_eq!(
            replay.subjects[&instance].attachment,
            super::super::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: target
            }
        );
        assert!(
            replay
                .composition
                .view_set(&owner.mount_manifest.surface_instance_id, target)
                .is_none(),
            "acknowledged CloseTab removed the exact target"
        );
        let mut session = crate::ui::reducer::test_support::session();
        session.binding_attachments = initial.binding_attachments.clone();
        let mut core = crate::ui::reducer::test_support::core(session);
        core.presentation = Some(replay.composition);
        core.selection_attachments.insert(
            instance.clone(),
            replay.subjects[&instance].attachment.clone(),
        );
        assert!(
            !super::super::attachment::selection_dependencies(
                core.binding_for_instance(&instance, view_ref)
                    .expect("exact matching admitted View")
            )
            .is_empty()
        );
        core.seat.append_facet(
            super::super::seat::selection_storage_key(follower, "selection.work").unwrap(),
            serde_json::json!({"thread":"T-own-must-not-borrow"}),
        );
        assert!(core.followed_selection_view_set(&instance).is_none());
        assert!(
            core.facet_value_for_instance(&instance, "selection.work")
                .is_none()
        );
    }

    #[test]
    fn initialization_is_the_only_replay_base_and_closure_does_not_recycle_ids() {
        let initial = initial();
        let surface = &initial.surfaces[0];
        let events = vec![
            event(&initial),
            SeatEvent {
                producer_engine_seq: None,
                seq: 7,
                kind: SeatEventKind::SurfaceClosed(super::super::seat::UiSurfaceClosure {
                    presentation_id: initial.presentation_id.clone(),
                    surface_instance_id: surface.surface_instance_id.clone(),
                    binding: surface.binding.clone(),
                }),
            },
        ];
        let replay = UiCompositionReplay::from_history(&initial, &events).unwrap();
        assert!(replay.composition.surfaces().is_empty());
        assert!(!replay.composition.dialogs().is_empty());
        assert_eq!(replay.next_mount_id, initial.next_mount_id);
        assert_eq!(replay.revision, 1);
        assert!(UiCompositionReplay::from_history(&initial, &events[1..]).is_err());
        let mut duplicate = events.clone();
        duplicate.push(SeatEvent {
            producer_engine_seq: None,
            seq: 9,
            kind: event(&initial).kind,
        });
        assert!(UiCompositionReplay::from_history(&initial, &duplicate).is_err());
        let mut reordered = events;
        reordered[1].seq = 3;
        assert!(UiCompositionReplay::from_history(&initial, &reordered).is_err());
    }
}
