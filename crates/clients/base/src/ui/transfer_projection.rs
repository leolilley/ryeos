//! Retained-seat-history-derived exact-instance fences. No registry or writer.
//!
//! Callers supply an authenticated lifecycle reconstructed from retained CAS.
//! This projection is disposable and must never stand in for durable testimony.
//! It does not itself enforce Core mutations, effect starts or delivery; those
//! callers must install authenticated restrictions before transfer replay opens.
use super::{
    transfer_export::{UiTransferExport, UiTransferExportMode},
    transfer_reservation::{UiTransferDestinationPlacement, UiTransferReservation},
};
use crate::ids::{RyeOsViewInstanceKey, ViewSetId};
use ryeos_ui_presentation_contract::transfer_state::{
    TransferDestinationState, TransferLifecycleState, TransferSourceState,
};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq)]
pub struct UiTransferFenceProjection {
    source_state: TransferSourceState,
    destination_state: TransferDestinationState,
    mode: UiTransferExportMode,
    source_mounts: BTreeSet<RyeOsViewInstanceKey>,
    source_sets: BTreeSet<ViewSetId>,
    destination_mounts: BTreeSet<RyeOsViewInstanceKey>,
    destination_sets: BTreeSet<ViewSetId>,
}

impl UiTransferFenceProjection {
    pub fn from_lifecycle(
        lifecycle: &TransferLifecycleState,
        max_bytes: u64,
    ) -> Result<Self, String> {
        let prepared = lifecycle
            .prepared()
            .ok_or("destination-only inbound commitment has no source preparation")?
            .record;
        let export = UiTransferExport::from_preparation(prepared, max_bytes)?;
        let source_mounts = export
            .mounts()
            .into_iter()
            .map(|mount| mount.source_instance.clone())
            .collect();
        let source_sets = export
            .sets()
            .into_iter()
            .flatten()
            .map(|set| set.source_view_set_id)
            .collect();
        let mut destination_mounts = BTreeSet::new();
        let mut destination_sets = BTreeSet::new();
        if let Some(accepted) = lifecycle.accepted() {
            let reservation = UiTransferReservation::from_acceptance(accepted.record, max_bytes)?;
            reservation.validate_export(lifecycle.transfer(), &export, max_bytes)?;
            destination_mounts.extend(
                reservation
                    .mounts
                    .iter()
                    .map(|mount| mount.destination_instance.clone()),
            );
            if let UiTransferDestinationPlacement::Tabs { manifest, .. } = reservation.placement {
                destination_sets.extend(manifest.tabs.into_iter().map(|tab| tab.view_set_id));
            }
        }
        Ok(Self {
            source_state: lifecycle.source_state(),
            destination_state: lifecycle.destination_state(),
            mode: export.mode,
            source_mounts,
            source_sets,
            destination_mounts,
            destination_sets,
        })
    }

    pub fn source_instance_frozen(&self, instance: &RyeOsViewInstanceKey) -> bool {
        self.source_state == TransferSourceState::Frozen && self.source_mounts.contains(instance)
    }

    pub fn source_set_frozen(&self, set: ViewSetId) -> bool {
        self.source_state == TransferSourceState::Frozen && self.source_sets.contains(&set)
    }

    /// A committed MOVE cannot revive its old exact instance. COPY settles its
    /// preparation fence without detaching the independently owned source.
    pub fn source_instance_retired(&self, instance: &RyeOsViewInstanceKey) -> bool {
        self.mode == UiTransferExportMode::Move
            && self.source_state == TransferSourceState::Committed
            && self.source_mounts.contains(instance)
    }

    /// Empty exported sets have no mount through which to infer retirement.
    /// Their exact allocated set identity must settle with the same MOVE.
    pub fn source_set_retired(&self, set: ViewSetId) -> bool {
        self.mode == UiTransferExportMode::Move
            && self.source_state == TransferSourceState::Committed
            && self.source_sets.contains(&set)
    }

    pub fn destination_instance_reserved(&self, instance: &RyeOsViewInstanceKey) -> bool {
        self.destination_state == TransferDestinationState::Reserved
            && self.destination_mounts.contains(instance)
    }

    pub fn destination_set_reserved(&self, set: ViewSetId) -> bool {
        self.destination_state == TransferDestinationState::Reserved
            && self.destination_sets.contains(&set)
    }

    /// A released reservation is never a live mount and its identities must
    /// not be recycled. Keeping this separate avoids treating abort as activate.
    pub fn destination_instance_retired(&self, instance: &RyeOsViewInstanceKey) -> bool {
        self.destination_state == TransferDestinationState::Released
            && self.destination_mounts.contains(instance)
    }

    pub fn destination_set_retired(&self, set: ViewSetId) -> bool {
        self.destination_state == TransferDestinationState::Released
            && self.destination_sets.contains(&set)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ids::{PresentationId, SurfaceInstanceId, TileId, UiViewMountOwner, ViewGroupId},
        surface::view_sets::ViewSetSeedSpec,
        ui::{
            attachment::SelectionAttachment,
            binding::UiBindingAttachmentCoordinate,
            composition_delta::UiCompositionMountAllocation,
            composition_operation::{CompositionPlacement, UiCompositionRouteAssignment},
            transfer_export::{
                UI_TRANSFER_EXPORT_SCHEMA, UiTransferExportContent, UiTransferMountExport,
                UiTransferSetExport,
            },
            transfer_reservation::UiTransferMountAllocation,
        },
        view_set::ViewLocalState,
    };
    use ryeos_ui_presentation_contract::{
        payload_digest::seat_payload_digest,
        transfer::{
            TRANSFER_SCHEMA, TransferEvidence, TransferIdentity, TransferPhase, TransferRecord,
            TransferSeat,
        },
        transfer_proof::TransferTestimony,
    };

    struct Fixture {
        inbound: TransferRecord,
        prepared: TransferRecord,
        accepted: TransferRecord,
        committed: TransferRecord,
        aborted: TransferRecord,
        released: TransferRecord,
        inbound_evidence: TransferEvidence,
        prepared_evidence: TransferEvidence,
        accepted_evidence: TransferEvidence,
        committed_evidence: TransferEvidence,
        aborted_evidence: TransferEvidence,
        released_evidence: TransferEvidence,
        source: RyeOsViewInstanceKey,
        destination: RyeOsViewInstanceKey,
    }

    fn fixture(mode: UiTransferExportMode) -> Fixture {
        let source_presentation =
            PresentationId::try_new("11111111-1111-4111-8111-111111111111").unwrap();
        let destination_presentation =
            PresentationId::try_new("22222222-2222-4222-8222-222222222222").unwrap();
        let source_surface = SurfaceInstanceId::try_new("source-surface").unwrap();
        let destination_surface = SurfaceInstanceId::try_new("destination-surface").unwrap();
        let source = RyeOsViewInstanceKey::surface_tile(
            &source_presentation,
            &source_surface,
            TileId::new(7),
        );
        let destination = RyeOsViewInstanceKey::surface_tile(
            &destination_presentation,
            &destination_surface,
            TileId::new(20),
        );
        let source_binding = UiBindingAttachmentCoordinate {
            binding_attachment_id: "source-binding".into(),
            binding_generation: 1,
            binding_digest: "ab".repeat(32),
        };
        let destination_binding = UiBindingAttachmentCoordinate {
            binding_attachment_id: "destination-binding".into(),
            binding_generation: 2,
            binding_digest: "bc".repeat(32),
        };
        let export = UiTransferExport {
            schema: UI_TRANSFER_EXPORT_SCHEMA,
            mode,
            content: UiTransferExportContent::SurfaceMount {
                source_view_set_id: ViewSetId::new(3),
                mount: UiTransferMountExport {
                    source_instance: source.clone(),
                    view_ref: "view:test/one".into(),
                    binding: source_binding,
                    local: ViewLocalState::None,
                    subject: SelectionAttachment::RequiredSubject {
                        input: "subject".into(),
                        facets: vec!["selection.work".into()],
                    },
                    route: UiCompositionRouteAssignment::Authored,
                    current_label: None,
                    drafts: vec![],
                    lens_frames: vec![],
                },
            },
        };
        let allocation = UiCompositionMountAllocation::allocate(20, 1, 0).unwrap();
        let descriptor = serde_json::from_value(serde_json::json!({
            "binding_attachment_id": destination_binding.binding_attachment_id,
            "binding_generation": destination_binding.binding_generation,
            "binding_digest": destination_binding.binding_digest,
            "root_ref": "surface:test", "root_generation": "generation-one",
            "presentation": {"definition": {"kind":"surface", "surface":{"name":"test"}}, "views":{}},
            "project_path": null, "subject": {"kind":"projectless"},
            "posture": "observation_only",
            "binding_request_bounds": {"max_request_bytes":4096,"max_input_bytes":1024,"max_seat_append_bytes":65536}
        })).unwrap();
        let reservation = UiTransferReservation {
            schema: 2,
            presentation_id: destination_presentation.clone(),
            expected_revision: 4,
            allocation_start: 20,
            next_mount_id: allocation.next_mount_id,
            placement: UiTransferDestinationPlacement::View {
                surface_instance_id: destination_surface,
                view_set_id: ViewSetId::new(3),
                allocation,
                placement: CompositionPlacement::GroupTab {
                    group_id: ViewGroupId::new(4),
                    before: None,
                },
            },
            mounts: vec![UiTransferMountAllocation {
                lens_frames: vec![],
                source_instance: source.clone(),
                destination_instance: destination.clone(),
                binding: destination_binding,
                destination_subject: SelectionAttachment::RequiredSubject {
                    input: "subject".into(),
                    facets: vec!["selection.work".into()],
                },
            }],
            descriptors: vec![descriptor],
        };
        let export_value = serde_json::to_value(export).unwrap();
        let reservation_value = serde_json::to_value(reservation).unwrap();
        let import_digest = seat_payload_digest(&reservation_value).unwrap();
        let seat = |presentation_id: &PresentationId, side: &str| TransferSeat {
            presentation_id: presentation_id.as_str().to_owned(),
            thread_id: format!("thread-{side}"),
            chain_root_id: format!("chain-{side}"),
            producer_incarnation: "de".repeat(32),
            revision: 4,
        };
        let transfer = TransferIdentity {
            operation_id: "33333333-3333-4333-8333-333333333333".into(),
            source: seat(&source_presentation, "source"),
            destination: seat(&destination_presentation, "destination"),
            export_digest: seat_payload_digest(&export_value).unwrap(),
        };
        let evidence = |marker: &str, chain_seq| TransferEvidence {
            event_hash: marker.repeat(32),
            chain_seq,
        };
        let inbound_evidence = evidence("00", 1);
        let prepared_evidence = evidence("aa", 3);
        let accepted_evidence = evidence("bb", 2);
        let committed_evidence = evidence("cc", 4);
        let aborted_evidence = evidence("dd", 4);
        let released_evidence = evidence("ee", 5);
        let record = |phase| TransferRecord {
            schema: TRANSFER_SCHEMA,
            transfer: transfer.clone(),
            phase,
        };
        Fixture {
            inbound: record(TransferPhase::InboundCommitment),
            prepared: record(TransferPhase::Prepared {
                inbound: inbound_evidence.clone(),
                // Shape-only lifecycle fixture, not authenticated receipt proof.
                freeze: (mode == UiTransferExportMode::Move).then(|| {
                    ryeos_ui_presentation_contract::transfer::MoveFreezeEvidence {
                        preparation: evidence("01", 1),
                        completion: evidence("02", 2),
                    }
                }),
                export: export_value,
            }),
            accepted: record(TransferPhase::Accepted {
                preparation: prepared_evidence.clone(),
                reservation: reservation_value,
                import_digest: import_digest.clone(),
            }),
            committed: record(TransferPhase::Committed {
                preparation: prepared_evidence.clone(),
                acceptance: accepted_evidence.clone(),
                import_digest: import_digest.clone(),
            }),
            aborted: record(TransferPhase::Aborted {
                preparation: prepared_evidence.clone(),
            }),
            released: record(TransferPhase::Released {
                abort: aborted_evidence.clone(),
                acceptance: accepted_evidence.clone(),
                import_digest,
            }),
            inbound_evidence,
            prepared_evidence,
            accepted_evidence,
            committed_evidence,
            aborted_evidence,
            released_evidence,
            source,
            destination,
        }
    }

    fn testimony<'a>(
        record: &'a TransferRecord,
        evidence: &'a TransferEvidence,
    ) -> TransferTestimony<'a> {
        TransferTestimony { record, evidence }
    }

    fn lifecycle(fixture: &Fixture) -> TransferLifecycleState {
        let mut lifecycle =
            TransferLifecycleState::begin(testimony(&fixture.inbound, &fixture.inbound_evidence))
                .unwrap();
        lifecycle
            .fold(testimony(&fixture.prepared, &fixture.prepared_evidence))
            .unwrap();
        lifecycle
    }

    #[test]
    fn destination_only_inbound_commitment_projects_no_mount_fence() {
        let fixture = fixture(UiTransferExportMode::Move);
        let orphan =
            TransferLifecycleState::begin(testimony(&fixture.inbound, &fixture.inbound_evidence))
                .unwrap();
        assert!(UiTransferFenceProjection::from_lifecycle(&orphan, 64 * 1024).is_err());
    }

    #[test]
    fn prepare_freezes_only_the_exact_source_and_abort_unfreezes_it() {
        let fixture = fixture(UiTransferExportMode::Move);
        let mut lifecycle = lifecycle(&fixture);
        let projection = UiTransferFenceProjection::from_lifecycle(&lifecycle, 64 * 1024).unwrap();
        assert!(projection.source_instance_frozen(&fixture.source));
        let unrelated = RyeOsViewInstanceKey::surface_tile(
            &PresentationId::try_new("11111111-1111-4111-8111-111111111111").unwrap(),
            &SurfaceInstanceId::try_new("other-surface").unwrap(),
            TileId::new(7),
        );
        assert!(!projection.source_instance_frozen(&unrelated));

        lifecycle
            .fold(testimony(&fixture.aborted, &fixture.aborted_evidence))
            .unwrap();
        let projection = UiTransferFenceProjection::from_lifecycle(&lifecycle, 64 * 1024).unwrap();
        assert!(!projection.source_instance_frozen(&fixture.source));
        assert!(!projection.source_instance_retired(&fixture.source));
    }

    #[test]
    fn empty_set_owner_must_match_the_prepared_source_presentation() {
        let mut fixture = fixture(UiTransferExportMode::Move);
        let source_owner = UiViewMountOwner::Surface {
            presentation_id: PresentationId::try_new("11111111-1111-4111-8111-111111111111")
                .unwrap(),
            surface_instance_id: SurfaceInstanceId::try_new("source-surface").unwrap(),
        };
        let export = UiTransferExport {
            schema: UI_TRANSFER_EXPORT_SCHEMA,
            mode: UiTransferExportMode::Move,
            content: UiTransferExportContent::ViewSets {
                active_set: None,
                sets: vec![UiTransferSetExport {
                    focused_view: None,
                    handle: "empty".into(),
                    source_view_set_id: ViewSetId::new(3),
                    source_owner,
                    binding: UiBindingAttachmentCoordinate {
                        binding_attachment_id: "source-binding".into(),
                        binding_generation: 1,
                        binding_digest: "ab".repeat(32),
                    },
                    seed: ViewSetSeedSpec {
                        schema: crate::surface::view_sets::VIEW_SET_COMPOSITION_SCHEMA,
                        id: "empty".into(),
                        title: "Empty".into(),
                        root: None,
                    },
                    mounts: vec![],
                }],
            },
        };
        let mut export = serde_json::to_value(export).unwrap();
        fixture.prepared.transfer.export_digest = seat_payload_digest(&export).unwrap();
        fixture.inbound.transfer.export_digest = fixture.prepared.transfer.export_digest.clone();
        fixture.prepared.phase = TransferPhase::Prepared {
            inbound: fixture.inbound_evidence.clone(),
            freeze: Some(
                ryeos_ui_presentation_contract::transfer::MoveFreezeEvidence {
                    preparation: TransferEvidence {
                        event_hash: "01".repeat(32),
                        chain_seq: 1,
                    },
                    completion: TransferEvidence {
                        event_hash: "02".repeat(32),
                        chain_seq: 2,
                    },
                },
            ),
            export: export.clone(),
        };
        let initial_lifecycle = lifecycle(&fixture);
        let projection =
            UiTransferFenceProjection::from_lifecycle(&initial_lifecycle, 64 * 1024).unwrap();
        assert!(projection.source_set_frozen(ViewSetId::new(3)));

        export["content"]["sets"][0]["source_owner"]["presentation_id"] =
            serde_json::json!("44444444-4444-4444-8444-444444444444");
        fixture.prepared.transfer.export_digest = seat_payload_digest(&export).unwrap();
        fixture.inbound.transfer.export_digest = fixture.prepared.transfer.export_digest.clone();
        fixture.prepared.phase = TransferPhase::Prepared {
            inbound: fixture.inbound_evidence.clone(),
            freeze: Some(
                ryeos_ui_presentation_contract::transfer::MoveFreezeEvidence {
                    preparation: TransferEvidence {
                        event_hash: "01".repeat(32),
                        chain_seq: 1,
                    },
                    completion: TransferEvidence {
                        event_hash: "02".repeat(32),
                        chain_seq: 2,
                    },
                },
            ),
            export,
        };
        let lifecycle = lifecycle(&fixture);
        assert!(UiTransferFenceProjection::from_lifecycle(&lifecycle, 64 * 1024).is_err());
    }

    #[test]
    fn committed_move_retires_source_but_committed_copy_does_not() {
        for (mode, retired) in [
            (UiTransferExportMode::Move, true),
            (UiTransferExportMode::Copy, false),
        ] {
            let fixture = fixture(mode);
            let mut lifecycle = lifecycle(&fixture);
            lifecycle
                .fold(testimony(&fixture.accepted, &fixture.accepted_evidence))
                .unwrap();
            lifecycle
                .fold(testimony(&fixture.committed, &fixture.committed_evidence))
                .unwrap();
            let projection =
                UiTransferFenceProjection::from_lifecycle(&lifecycle, 64 * 1024).unwrap();
            assert_eq!(projection.source_instance_retired(&fixture.source), retired);
            assert!(!projection.source_instance_frozen(&fixture.source));
        }
    }

    #[test]
    fn accepted_destination_stays_reserved_through_abort_until_exact_release() {
        let fixture = fixture(UiTransferExportMode::Move);
        let mut lifecycle = lifecycle(&fixture);
        lifecycle
            .fold(testimony(&fixture.accepted, &fixture.accepted_evidence))
            .unwrap();
        let projection = UiTransferFenceProjection::from_lifecycle(&lifecycle, 64 * 1024).unwrap();
        assert!(projection.destination_instance_reserved(&fixture.destination));
        let unrelated = RyeOsViewInstanceKey::surface_tile(
            &PresentationId::try_new("22222222-2222-4222-8222-222222222222").unwrap(),
            &SurfaceInstanceId::try_new("other-surface").unwrap(),
            TileId::new(20),
        );
        assert!(!projection.destination_instance_reserved(&unrelated));

        lifecycle
            .fold(testimony(&fixture.aborted, &fixture.aborted_evidence))
            .unwrap();
        let projection = UiTransferFenceProjection::from_lifecycle(&lifecycle, 64 * 1024).unwrap();
        assert!(projection.destination_instance_reserved(&fixture.destination));
        assert!(!projection.destination_instance_retired(&fixture.destination));

        lifecycle
            .fold(testimony(&fixture.released, &fixture.released_evidence))
            .unwrap();
        let projection = UiTransferFenceProjection::from_lifecycle(&lifecycle, 64 * 1024).unwrap();
        assert!(!projection.destination_instance_reserved(&fixture.destination));
        assert!(projection.destination_instance_retired(&fixture.destination));
    }
}
