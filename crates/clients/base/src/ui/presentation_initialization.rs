//! Immutable admission testimony written once by the existing seat owner.
//!
//! This event is outside the producer's local append interval. Its descriptors
//! are retained evidence, not replacement grants or permission to revive a
//! closed Surface. Current session membership remains the dispatch boundary.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::binding::{UiBindingAttachment, UiBindingAttachmentCoordinate, UiSurfaceMembership};

pub const PRESENTATION_INITIALIZED_EVENT: &str = "ui_seat.presentation_initialized.v1";

/// Indexed under the original Prepare identity, not a producer append UUID.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiPresentationInitializationRecord {
    pub operation_id: String,
    pub initialization: UiPresentationInitialization,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{PresentationId, SurfaceInstanceId};
    use crate::ui::composition_identity::UiSurfaceMountManifest;
    const PRESENTATION: &str = "11111111-1111-4111-8111-111111111111";
    const PREPARE: &str = "22222222-2222-4222-8222-222222222222";
    fn descriptor(id: &str, surface: bool) -> UiBindingAttachment {
        let presentation = if surface {
            serde_json::json!({"definition":{"kind":"surface","surface":{"name":"test-work","title":"Work","tiles":["view:test/work"]}},"views":{"view:test/work":{}}})
        } else {
            serde_json::json!({"definition":{"kind":"shared_presentation","config":{"category":"ui","shared_presentation":{"schema":1,"title":"RyeOS","launcher":"launch","dialogs":[{"id":"launch","title":"Launch","view":"view:test/work"}],"capabilities":{"sources":[],"affordances":[]}}}},"views":{"view:test/work":{}}})
        };
        serde_json::from_value(serde_json::json!({"binding_attachment_id":id,"binding_generation":1,"binding_digest":"a".repeat(64),"root_ref":if surface{"surface:test/work"}else{"config:test/shared"},"root_generation":"generation","project_path":null,"subject":{"kind":"projectless"},"posture":"observation_only","binding_request_bounds":{"max_request_bytes":4096,"max_input_bytes":1024,"max_seat_append_bytes":65536},"presentation":presentation})).unwrap()
    }
    fn coordinate(value: &UiBindingAttachment) -> UiBindingAttachmentCoordinate {
        UiBindingAttachmentCoordinate {
            binding_attachment_id: value.binding_attachment_id.clone(),
            binding_generation: value.binding_generation,
            binding_digest: value.binding_digest.clone(),
        }
    }
    fn initialization(count: usize) -> UiPresentationInitialization {
        let root = descriptor("root", false);
        let mut result = UiPresentationInitialization {
            schema: 2,
            selection: UiPresentationInitialSelection::ConfiguredDefaults,
            presentation_id: PRESENTATION.into(),
            creation_request_id: PREPARE.into(),
            root_binding: coordinate(&root),
            next_mount_id: 1,
            surfaces: Vec::new(),
            binding_attachments: vec![root],
        };
        for index in 0..count {
            let attachment = descriptor(&format!("surface-{index}"), true);
            let id = format!("33333333-3333-4333-8333-{:012}", index + 1);
            let seeds = attachment
                .presentation
                .definition
                .surface()
                .unwrap()
                .to_mount_seeds()
                .unwrap();
            let active = seeds[0].id.clone();
            let manifest = UiSurfaceMountManifest::allocate(
                PresentationId::try_new(PRESENTATION).unwrap(),
                SurfaceInstanceId::try_new(id.clone()).unwrap(),
                result.next_mount_id,
                seeds
                    .into_iter()
                    .map(|seed| (seed, coordinate(&attachment)))
                    .collect(),
                Some(&active),
            )
            .unwrap();
            result.next_mount_id = manifest.next_mount_id;
            result.surfaces.push(UiSurfaceMembership {
                surface_instance_id: id,
                ordinal: 7 + index as u64,
                title: "Work".into(),
                binding: coordinate(&attachment),
                mount_manifest: manifest,
                saved_surface: None,
            });
            result.binding_attachments.push(attachment);
        }
        result
            .binding_attachments
            .sort_by(|a, b| a.binding_attachment_id.cmp(&b.binding_attachment_id));
        result
    }
    #[test]
    fn selected_initialization_binds_request_title_and_exact_descriptor_closure() {
        use crate::surface::opening::{
            SurfaceOpenPlacement, SurfaceOpenRequest, SurfaceOpenSelection,
        };
        let mut selected = initialization(1);
        selected.selection = UiPresentationInitialSelection::SelectedSurface {
            opening: SurfaceOpenRequest {
                creation_request_id: selected.creation_request_id.clone(),
                selection: SurfaceOpenSelection::Authored {
                    surface_ref: "surface:test/work".into(),
                },
                placement: SurfaceOpenPlacement::NewPresentation {
                    root_binding: selected.root_binding.clone(),
                },
            },
        };
        selected.validate().unwrap();
        let mut changed = selected.clone();
        changed.surfaces[0].title = "Different".into();
        assert!(changed.validate().is_err());
        let mut changed = selected.clone();
        changed
            .binding_attachments
            .push(descriptor("unrequested", true));
        changed
            .binding_attachments
            .sort_by(|left, right| left.binding_attachment_id.cmp(&right.binding_attachment_id));
        assert!(changed.validate().is_err());
        let mut changed = selected.clone();
        let UiPresentationInitialSelection::SelectedSurface { opening } = &mut changed.selection
        else {
            unreachable!()
        };
        opening.creation_request_id = "55555555-5555-4555-8555-555555555555".into();
        assert!(changed.validate().is_err());
        let mut changed = selected.clone();
        changed.surfaces.push(changed.surfaces[0].clone());
        assert!(changed.validate().is_err());
        let mut old = selected;
        old.schema = 1;
        assert!(old.validate().is_err());
    }

    #[test]
    fn valid_empty_and_multiple_initializations_have_stable_digest() {
        for count in [0, 2] {
            let value = initialization(count);
            value.validate().unwrap();
            let digest = value.digest().unwrap();
            for _ in 0..5 {
                let _ = crate::view_set::ViewSet::from_tiling(
                    crate::surface::TilingSpec::default(),
                    Vec::new(),
                    &crate::ids::PresentationId::try_new("allocation-fixture").unwrap(),
                    &crate::ids::SurfaceInstanceId::try_new("allocation-surface").unwrap(),
                );
            }
            assert_eq!(digest, initialization(count).digest().unwrap());
        }
        let mut changed = initialization(2);
        let before = changed.digest().unwrap();
        changed.surfaces[0].title = "Alias".into();
        assert_ne!(before, changed.digest().unwrap());
    }
    #[test]
    fn exact_closure_owner_and_manifest_are_required() {
        let valid = initialization(2);
        let mut missing = valid.clone();
        missing.binding_attachments.pop();
        assert!(missing.validate().is_err());
        let mut extra = valid.clone();
        extra.binding_attachments.push(descriptor("zz-extra", true));
        assert!(extra.validate().is_err());
        let mut changed = valid.clone();
        changed.binding_attachments[1].binding_generation += 1;
        assert!(changed.validate().is_err());
        let mut wrong = valid.clone();
        wrong.surfaces[0].mount_manifest.presentation_id =
            PresentationId::try_new(PREPARE).unwrap();
        assert!(wrong.validate().is_err());
        let mut triple = valid.clone();
        triple.surfaces[0].binding.binding_digest = "b".repeat(64);
        assert!(triple.validate().is_err());
        let mut seed = valid.clone();
        seed.surfaces[0].mount_manifest.tabs[0].seed.title = "Different".into();
        assert!(seed.validate().is_err());
        let mut frontier = valid.clone();
        frontier.next_mount_id += 1;
        assert!(frontier.validate().is_err());
        let mut duplicate = valid.clone();
        duplicate.surfaces[1].surface_instance_id =
            duplicate.surfaces[0].surface_instance_id.clone();
        assert!(duplicate.validate().is_err());
        let mut ordinal = valid.clone();
        ordinal.surfaces[1].ordinal = ordinal.surfaces[0].ordinal;
        assert!(ordinal.validate().is_err());
        let mut order = valid.clone();
        order.binding_attachments.swap(0, 1);
        assert!(order.validate().is_err());
        assert!(
            UiPresentationInitializationRecord {
                operation_id: PRESENTATION.into(),
                initialization: valid
            }
            .validate()
            .is_err()
        );
    }

    fn core(value: &UiPresentationInitialization) -> super::super::model::RyeOsCore {
        let session = super::super::model::BrowserSession {
            ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION.into(),
            session_id: "session".into(),
            root_attachment_id: value.root_binding.binding_attachment_id.clone(),
            presentations: vec![super::super::binding::UiPresentationMembership {
                presentation_id: value.presentation_id.clone(),
                root_binding: value.root_binding.clone(),
                thread_id: Some("T-seat".into()),
                chain_root_id: Some("T-seat".into()),
                next_mount_id: value.next_mount_id,
                initialization_digest: value.digest().unwrap(),
                surfaces: value.surfaces.clone(),
            }],
            binding_attachments: value.binding_attachments.clone(),
            user_principal_id: None,
            events_url: None,
        };
        super::super::model::RyeOsCore::new(session, PRESENTATION, Default::default(), 0)
    }

    #[test]
    fn core_requires_exact_singleton_initialization_before_effects() {
        let value = initialization(2);
        let mut core = core(&value);
        assert!(core.require_replayed_initialization().is_err());
        let event = super::super::seat::SeatEvent {
            producer_engine_seq: None,
            seq: 5,
            kind: super::super::seat::SeatEventKind::PresentationInitialized(
                UiPresentationInitializationRecord {
                    operation_id: PREPARE.into(),
                    initialization: value,
                },
            ),
        };
        core.validate_replayed_initialization(std::slice::from_ref(&event))
            .unwrap();
        core.replay_seat_events([event.clone()]);
        core.require_replayed_initialization().unwrap();
        core.validate_replayed_initialization(std::slice::from_ref(&event))
            .unwrap();
        let mut duplicate = event.clone();
        duplicate.seq += 1;
        assert!(core.validate_replayed_initialization(&[duplicate]).is_err());
        let replacement = super::super::seat::SeatEvent {
            producer_engine_seq: None,
            seq: event.seq,
            kind: super::super::seat::SeatEventKind::Facet {
                key: "replacement".into(),
                value: serde_json::Value::Null,
            },
        };
        assert!(
            core.validate_replayed_initialization(&[replacement])
                .is_err()
        );
        core.presentation_operations.replay_blocked = true;
        assert!(core.require_replayed_initialization().is_err());
    }
}

impl UiPresentationInitializationRecord {
    pub fn validate(&self) -> Result<(), String> {
        let payload = serde_json::to_value(self)
            .map_err(|_| "initialization identity encoding failed".to_owned())?;
        ryeos_ui_presentation_contract::initialization::UiPresentationInitializationIdentity::from_payload(&payload)
            .map_err(str::to_owned)?;
        self.initialization.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiPresentationInitialSelection {
    ConfiguredDefaults,
    SelectedSurface {
        opening: crate::surface::opening::SurfaceOpenRequest,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiPresentationInitialization {
    pub schema: u32,
    pub selection: UiPresentationInitialSelection,
    pub presentation_id: String,
    pub creation_request_id: String,
    pub root_binding: UiBindingAttachmentCoordinate,
    pub next_mount_id: u64,
    pub surfaces: Vec<UiSurfaceMembership>,
    pub binding_attachments: Vec<UiBindingAttachment>,
}

impl UiPresentationInitialization {
    pub fn digest(&self) -> Result<String, String> {
        let bytes = serde_json::to_vec(self).map_err(|error| error.to_string())?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema
            != ryeos_ui_presentation_contract::initialization::PRESENTATION_INITIALIZATION_SCHEMA
            || !super::admission_operation::canonical_operation_id(&self.presentation_id)
            || !super::admission_operation::canonical_operation_id(&self.creation_request_id)
        {
            return Err("presentation initialization identity is invalid".into());
        }
        let mut descriptors = BTreeMap::new();
        let mut previous = None;
        for attachment in &self.binding_attachments {
            attachment.validate().map_err(str::to_owned)?;
            if previous.is_some_and(|id: &str| id >= attachment.binding_attachment_id.as_str()) {
                return Err("initialization descriptors are not unique canonical order".into());
            }
            previous = Some(attachment.binding_attachment_id.as_str());
            descriptors.insert(attachment.binding_attachment_id.as_str(), attachment);
        }
        let exact = |coordinate: &UiBindingAttachmentCoordinate| {
            descriptors
                .get(coordinate.binding_attachment_id.as_str())
                .copied()
                .filter(|entry| {
                    entry.binding_generation == coordinate.binding_generation
                        && entry.binding_digest == coordinate.binding_digest
                })
        };
        let root = exact(&self.root_binding).ok_or("initialization root binding is absent")?;
        if !matches!(
            &root.presentation.definition,
            crate::presentation::UiCompositionDefinition::SharedPresentation { .. }
        ) {
            return Err("initialization root is not the admitted Shared Config".into());
        }
        if let UiPresentationInitialSelection::SelectedSurface { opening } = &self.selection {
            opening.validate()?;
            let crate::surface::opening::SurfaceOpenPlacement::NewPresentation { root_binding } =
                &opening.placement
            else {
                return Err(
                    "selected initialization requires explicit new-presentation placement".into(),
                );
            };
            if root_binding != &self.root_binding
                || opening.creation_request_id != self.creation_request_id
                || self.surfaces.len() != 1
            {
                return Err(
                    "selected initialization changes root, request or Surface count".into(),
                );
            }
            let surface = &self.surfaces[0];
            let origin = exact(&surface.binding).ok_or("selected initialization origin absent")?;
            let title = match &opening.selection {
                crate::surface::opening::SurfaceOpenSelection::Composed(composition) => {
                    composition.title.as_str()
                }
                crate::surface::opening::SurfaceOpenSelection::Authored { .. } => origin
                    .presentation
                    .definition
                    .surface()
                    .map(|declaration| declaration.display_title())
                    .unwrap_or_default(),
                // Library title is admitted by the principal-library owner and
                // retained in the exact signed initialization, not copied into
                // a second library record by the pure wire validator.
                crate::surface::opening::SurfaceOpenSelection::Reusable { .. } => {
                    surface.title.as_str()
                }
            };
            if surface.title != title
                || title.trim().is_empty()
                || title.chars().any(char::is_control)
            {
                return Err(
                    "selected initialization title differs from admitted composition".into(),
                );
            }
            if origin.binding_attachment_id == root.binding_attachment_id
                || surface.ordinal == 0
                || !super::admission_operation::canonical_operation_id(&surface.surface_instance_id)
            {
                return Err("selected initialization Surface identity is invalid".into());
            }
            let tabs = self
                .binding_attachments
                .iter()
                .filter(|descriptor| {
                    descriptor.binding_attachment_id != root.binding_attachment_id
                        && descriptor.binding_attachment_id != origin.binding_attachment_id
                })
                .cloned()
                .collect::<Vec<_>>();
            let (seeds, active) = super::binding::validate_surface_open_selection(
                &opening.selection,
                surface.saved_surface.as_ref(),
                origin,
                &tabs,
            )?;
            let expected = super::composition_identity::UiSurfaceMountManifest::allocate(
                crate::ids::PresentationId::try_new(&self.presentation_id)?,
                crate::ids::SurfaceInstanceId::try_new(&surface.surface_instance_id)?,
                1,
                seeds,
                active.as_deref(),
            )?;
            if expected != surface.mount_manifest || expected.next_mount_id != self.next_mount_id {
                return Err("selected initialization manifest or frontier differs".into());
            }
            return Ok(());
        }
        let mut required = BTreeSet::from([self.root_binding.binding_attachment_id.as_str()]);
        let mut surface_ids = BTreeSet::new();
        let mut next = 1;
        let mut previous_ordinal = 0;
        for surface in &self.surfaces {
            if surface.ordinal <= previous_ordinal
                || !super::admission_operation::canonical_operation_id(&surface.surface_instance_id)
                || !surface_ids.insert(surface.surface_instance_id.as_str())
                || surface.saved_surface.is_some()
            {
                return Err("initial Surface order or identity is invalid".into());
            }
            previous_ordinal = surface.ordinal;
            let descriptor = exact(&surface.binding).ok_or("initial Surface binding is absent")?;
            required.insert(surface.binding.binding_attachment_id.as_str());
            let declaration = descriptor
                .presentation
                .definition
                .surface()
                .ok_or("initial member is not a Surface")?;
            let seeds = declaration.to_mount_seeds()?;
            let manifest = &surface.mount_manifest;
            manifest.validate()?;
            if manifest.presentation_id.as_str() != self.presentation_id
                || manifest.surface_instance_id.as_str() != surface.surface_instance_id
                || manifest.allocation_start != next
                || manifest.tabs.len() != seeds.len()
                || !manifest
                    .tabs
                    .iter()
                    .zip(&seeds)
                    .all(|(tab, seed)| tab.seed == *seed && tab.binding == surface.binding)
            {
                return Err("initial Surface manifest contradicts its admission".into());
            }
            next = manifest.next_mount_id;
        }
        if next != self.next_mount_id || required.len() != descriptors.len() {
            return Err("initialization frontier or binding closure differs".into());
        }
        Ok(())
    }
}

impl super::model::RyeOsCore {
    /// Check retained testimony against authenticated membership before any
    /// adapter enables effects. Neither testimony nor its descriptors admits
    /// an attachment absent from that current membership.
    pub fn validate_replayed_initialization(
        &self,
        events: &[super::seat::SeatEvent],
    ) -> Result<(), String> {
        use super::seat::SeatEventKind;
        for event in events {
            if self
                .seat
                .event_at_sequence(event.seq)
                .is_some_and(|previous| {
                    matches!(&previous.kind, SeatEventKind::PresentationInitialized(_))
                        && previous != event
                })
            {
                return Err("retained initialization changed at its chain position".into());
            }
        }
        let mut record = None;
        let mut record_seq = None;
        for event in self.seat.events().iter().chain(events) {
            let SeatEventKind::PresentationInitialized(initialized) = &event.kind else {
                continue;
            };
            if let Some(previous) = record {
                if record_seq != Some(event.seq) || previous != initialized {
                    return Err("retained initialization is duplicate or contradictory".into());
                }
                continue;
            }
            initialized.validate()?;
            let presentation = self
                .presentation
                .as_ref()
                .ok_or("no admitted presentation")?;
            let membership =
                self.data
                    .session
                    .as_ref()
                    .and_then(|session| {
                        session.presentations.iter().find(|membership| {
                            membership.presentation_id == presentation.id().as_str()
                        })
                    })
                    .ok_or("admitted presentation membership is absent")?;
            if initialized.initialization.presentation_id != membership.presentation_id
                || initialized.initialization.root_binding != membership.root_binding
                || initialized.initialization.digest()? != membership.initialization_digest
                || initialized.initialization.next_mount_id > membership.next_mount_id
            {
                return Err("retained initialization differs from authenticated admission".into());
            }
            record = Some(initialized);
            record_seq = Some(event.seq);
        }
        Ok(())
    }

    pub fn require_replayed_initialization(&self) -> Result<(), String> {
        if self.presentation_operations.replay_blocked {
            return Err("presentation replay is refused".into());
        }
        self.validate_replayed_initialization(&[])?;
        if !self.seat.events().iter().any(|event| {
            matches!(
                event.kind,
                super::seat::SeatEventKind::PresentationInitialized(_)
            )
        }) {
            return Err("presentation initialization was not replayed".into());
        }
        Ok(())
    }
}
