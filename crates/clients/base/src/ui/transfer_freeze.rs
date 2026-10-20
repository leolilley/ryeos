//! Closed MOVE freeze testimony. Shape checks do not grant transfer authority.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use super::binding::UiBindingAttachmentCoordinate;
use super::transfer_export::{UiTransferExport, UiTransferExportContent, UiTransferExportMode};
use crate::ids::{RyeOsViewInstanceKey, ViewSetId};
use ryeos_ui_presentation_contract::transfer::TransferIdentity;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiMoveFreezeRequest {
    pub schema: u32,
    /// Derived presentation preparation identity, distinct from transfer identity.
    pub operation_id: String,
    pub transfer: TransferIdentity,
    pub root_binding: UiBindingAttachmentCoordinate,
    pub export: UiTransferExport,
    /// Canonically sorted, duplicate-free exact source closure.
    pub instances: Vec<RyeOsViewInstanceKey>,
    pub view_sets: Vec<ViewSetId>,
}

impl UiMoveFreezeRequest {
    pub fn validate(&self, max_bytes: u64) -> Result<(), String> {
        if self.schema != 1
            || !super::admission_operation::canonical_operation_id(&self.operation_id)
            || !super::admission_operation::canonical_operation_id(&self.transfer.operation_id)
            || self.root_binding.binding_attachment_id.is_empty()
            || self.root_binding.binding_generation == 0
            || self.root_binding.binding_digest.is_empty()
            || self.export.mode != UiTransferExportMode::Move
        {
            return Err("MOVE freeze has invalid closed identity or mode".into());
        }
        ryeos_ui_presentation_contract::transfer::TransferRecord {
            schema: ryeos_ui_presentation_contract::transfer::TRANSFER_SCHEMA,
            transfer: self.transfer.clone(),
            phase: ryeos_ui_presentation_contract::transfer::TransferPhase::InboundCommitment,
        }
        .validate_shape()?;
        self.export.validate_for(&self.transfer, max_bytes)?;
        let instances = self
            .export
            .mounts()
            .into_iter()
            .map(|mount| mount.source_instance.clone())
            .collect::<BTreeSet<_>>();
        let sets = match &self.export.content {
            UiTransferExportContent::ViewSets { sets, .. } => sets
                .iter()
                .map(|set| set.source_view_set_id)
                .collect::<BTreeSet<_>>(),
            UiTransferExportContent::SurfaceMount {
                source_view_set_id, ..
            } => BTreeSet::from([*source_view_set_id]),
            UiTransferExportContent::SharedDock { .. } => BTreeSet::new(),
        };
        if self.instances != instances.into_iter().collect::<Vec<_>>()
            || self.view_sets != sets.into_iter().collect::<Vec<_>>()
        {
            return Err("MOVE freeze inventory differs from its exact export closure".into());
        }
        if serde_json::to_vec(self)
            .map_err(|error| error.to_string())?
            .len() as u64
            > max_bytes
        {
            return Err("MOVE freeze exceeds its admitted request bound".into());
        }
        Ok(())
    }

    pub fn completion(&self) -> UiMoveFreezeCompletion {
        UiMoveFreezeCompletion {
            schema: 1,
            operation_id: self.operation_id.clone(),
            transfer_operation_id: self.transfer.operation_id.clone(),
            export_digest: self.transfer.export_digest.clone(),
            source_revision: self.transfer.source.revision,
        }
    }

    pub fn validate_snapshot(
        &self,
        replay: &super::composition_replay::UiCompositionReplay,
        max_bytes: u64,
    ) -> Result<(), String> {
        self.validate(max_bytes)?;
        self.export
            .validate_snapshot(&self.transfer, replay, max_bytes)
    }
}

/// Nonstructural receipt: source revision remains unchanged by freeze.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiMoveFreezeCompletion {
    pub schema: u32,
    pub operation_id: String,
    pub transfer_operation_id: String,
    pub export_digest: String,
    pub source_revision: u64,
}

impl UiMoveFreezeCompletion {
    pub fn validate_for(&self, request: &UiMoveFreezeRequest) -> Result<(), &'static str> {
        if self != &request.completion() {
            return Err("MOVE freeze completion differs from its exact preparation");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::transfer_export::UiTransferSetExport;
    use super::*;
    use crate::ids::{PresentationId, UiViewMountOwner};
    use crate::surface::view_sets::ViewSetSeedSpec;
    use ryeos_ui_presentation_contract::transfer::TransferSeat;

    fn request() -> UiMoveFreezeRequest {
        let source_id = "11111111-1111-4111-8111-111111111111";
        let binding = UiBindingAttachmentCoordinate {
            binding_attachment_id: "root".into(),
            binding_generation: 1,
            binding_digest: "ab".repeat(32),
        };
        let export = UiTransferExport {
            schema: 1,
            mode: UiTransferExportMode::Move,
            content: UiTransferExportContent::ViewSets {
                active_set: Some("empty".into()),
                sets: vec![UiTransferSetExport {
                    handle: "empty".into(),
                    source_view_set_id: ViewSetId::new(1),
                    source_owner: UiViewMountOwner::Surface {
                        presentation_id: PresentationId::try_new(source_id).unwrap(),
                        surface_instance_id: crate::ids::SurfaceInstanceId::try_new("surface")
                            .unwrap(),
                    },
                    binding: binding.clone(),
                    seed: ViewSetSeedSpec {
                        schema: crate::surface::view_sets::VIEW_SET_COMPOSITION_SCHEMA,
                        id: "empty".into(),
                        title: "Empty".into(),
                        root: None,
                    },
                    focused_view: None,
                    mounts: vec![],
                }],
            },
        };
        let digest =
            super::super::seat_digest::seat_payload_digest(&serde_json::to_value(&export).unwrap())
                .unwrap();
        let seat = |presentation: &str, thread: &str, root: &str| TransferSeat {
            presentation_id: presentation.into(),
            thread_id: thread.into(),
            chain_root_id: root.into(),
            producer_incarnation: "cd".repeat(32),
            revision: 7,
        };
        UiMoveFreezeRequest {
            schema: 1,
            operation_id: "22222222-2222-4222-8222-222222222222".into(),
            transfer: TransferIdentity {
                operation_id: "33333333-3333-4333-8333-333333333333".into(),
                source: seat(source_id, "source-thread", "source-root"),
                destination: seat(
                    "44444444-4444-4444-8444-444444444444",
                    "dest-thread",
                    "dest-root",
                ),
                export_digest: digest,
            },
            root_binding: binding,
            export,
            instances: vec![],
            view_sets: vec![ViewSetId::new(1)],
        }
    }

    #[test]
    fn exact_inventory_and_nonstructural_completion_are_closed() {
        let mut request = request();
        request.validate(64 * 1024).unwrap();
        let mut completion = request.completion();
        completion.validate_for(&request).unwrap();
        completion.source_revision += 1;
        assert!(completion.validate_for(&request).is_err());
        request.view_sets.push(ViewSetId::new(1));
        assert!(request.validate(64 * 1024).is_err());
    }

    #[test]
    fn preparation_identity_retains_exact_freeze_without_sequence_adjacency() {
        let request = request();
        let mut prepared = super::super::admission_operation::UiPreparedPresentationOperation {
            schema: 2,
            operation_id: String::new(),
            producer: super::super::binding::UiPresentationProducer {
                presentation_id: request.transfer.source.presentation_id.clone(),
                thread_id: request.transfer.source.thread_id.clone(),
                producer_incarnation: request.transfer.source.producer_incarnation.clone(),
            },
            prepared_engine_seq: 4096,
            root_binding: request.root_binding.clone(),
            body:
                super::super::admission_operation::UiPreparedPresentationOperationBody::MoveFreeze {
                    request,
                },
            request_bounds: super::super::binding::UiBindingRequestBounds {
                max_request_bytes: 64 * 1024,
                max_input_bytes: 64 * 1024,
                max_seat_append_bytes: 64 * 1024,
            },
        };
        prepared.assign_identity().unwrap();
        prepared.validate().unwrap();
        let completed = super::super::admission_operation::UiCompletedPresentationOperation {
            operation_id: prepared.operation_id.clone(),
            result: serde_json::to_value(prepared.move_freeze().unwrap().completion()).unwrap(),
        };
        completed.validate_for(&prepared).unwrap();
        prepared.prepared_engine_seq += 10;
        assert!(prepared.validate_identity().is_err());
        assert!(
            serde_json::from_value::<UiMoveFreezeCompletion>(
                serde_json::json!({"schema":1,"unexpected":true})
            )
            .is_err()
        );
    }
}
