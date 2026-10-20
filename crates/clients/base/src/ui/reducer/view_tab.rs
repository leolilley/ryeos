//! Single-View opening uses the existing retained InsertTab mutation owner.
use std::collections::BTreeMap;

use crate::ids::{SurfaceInstanceId, ViewSetId};
use crate::surface::view_sets::{
    LayoutSeedSpec, SavedViewMountRef, SavedViewSelectionRelationship, SavedViewSelectionSource,
    SavedViewSetTemplate, VIEW_SET_COMPOSITION_SCHEMA, ViewSetSeedSpec,
};
use crate::ui::{
    binding::UiBindingAttachmentCoordinate, effect::RyeOsEffect,
    layout_preferences::RequiredSubjectMountPolicy, model::RyeOsCore,
    presentation_state::PresentationInsertionDestination, view_model::RyeOsTone,
};

impl RyeOsCore {
    pub(crate) fn open_surface_in_new_presentation(
        &mut self,
        surface_instance_id: SurfaceInstanceId,
        binding: UiBindingAttachmentCoordinate,
        dialog_id: String,
        dialog_binding: UiBindingAttachmentCoordinate,
    ) -> Vec<RyeOsEffect> {
        let capture = (|| -> Result<_, String> {
            if !self.supports_independent_documents || !self.composition_write_available() {
                return Err("independent presentation opening unavailable".into());
            }
            let surface = self
                .active_surface()
                .filter(|surface| {
                    surface.id() == &surface_instance_id && surface.binding() == &binding
                })
                .ok_or("captured Surface changed")?;
            let stable_origin = |coordinate: &UiBindingAttachmentCoordinate| -> Result<_, String> {
                let descriptor = self
                    .binding_attachment(&coordinate.binding_attachment_id)
                    .filter(|entry| {
                        entry.binding_generation == coordinate.binding_generation
                            && entry.binding_digest == coordinate.binding_digest
                            && entry.presentation.definition.surface().is_some()
                    })
                    .ok_or("Surface origin changed")?;
                Ok(crate::surface::saved_surfaces::SavedSurfaceOrigin {
                    surface_ref: descriptor.root_ref.clone(),
                    project: Self::saved_surface_project(&descriptor.subject)?,
                })
            };
            let ids = surface
                .view_sets()
                .iter()
                .enumerate()
                .map(|(index, set)| (set.id, format!("tab-{index}")))
                .collect::<BTreeMap<_, _>>();
            let active = surface
                .active_view_set()
                .and_then(|set| ids.get(&set.id))
                .cloned()
                .ok_or("Surface has no active tab")?;
            let mut tabs = Vec::new();
            for set in surface.view_sets() {
                let descriptor = self
                    .insertion_attachment_id(set.id)
                    .and_then(|id| self.binding_attachment(id))
                    .ok_or("tab origin unavailable")?;
                let origin = stable_origin(&UiBindingAttachmentCoordinate {
                    binding_attachment_id: descriptor.binding_attachment_id.clone(),
                    binding_generation: descriptor.binding_generation,
                    binding_digest: descriptor.binding_digest.clone(),
                })?;
                let template = self.capture_view_set_template_at(
                    set.id,
                    ids[&set.id].clone(),
                    set.title.clone(),
                    None,
                    &ids,
                )?;
                tabs.push(crate::surface::opening::ComposedSurfaceTab {
                    origin,
                    template,
                    fresh_subjects: BTreeMap::new(),
                });
            }
            let selection = crate::surface::opening::ComposedSurfaceSelection {
                origin: stable_origin(&binding)?,
                title: surface.title().into(),
                active_view_set_id: active,
                tabs,
            };
            selection.validate()?;
            let presentation = self
                .presentation
                .as_ref()
                .ok_or("presentation unavailable")?;
            let dialog = presentation
                .dialogs()
                .get(&dialog_id)
                .filter(|entry| {
                    entry.surface_owner().is_none() && presentation.dialog_available(entry)
                })
                .ok_or("shared opener unavailable")?;
            let instance = dialog.view().instance_key().clone();
            let descriptor = self
                .binding_attachment_for_instance(&instance)
                .ok_or("shared binding unavailable")?;
            if descriptor.binding_attachment_id != dialog_binding.binding_attachment_id
                || descriptor.binding_generation != dialog_binding.binding_generation
                || descriptor.binding_digest != dialog_binding.binding_digest
            {
                return Err("shared binding changed".into());
            }
            let view_ref = dialog.view().view_ref().to_owned();
            if !self
                .binding_for_instance(&instance, &view_ref)
                .is_some_and(|view| {
                    view.affordances.iter().any(|entry| {
                        entry.get("id").and_then(serde_json::Value::as_str)
                            == Some("open-composed-document")
                    })
                })
            {
                return Err("signed document opener unavailable".into());
            }
            let placement = crate::surface::opening::SurfaceOpenPlacement::NewPresentation {
                root_binding: presentation.root_binding().clone(),
            };
            Ok((instance, view_ref, selection, placement))
        })();
        let Ok((instance, view_ref, selection, placement)) = capture else {
            return Vec::new();
        };
        let mut effects = self.open_dialog(&dialog_id);
        effects.extend(self.invoke_affordance(&instance,&view_ref,"open-composed-document",&serde_json::json!({"opening_selection":crate::surface::opening::SurfaceOpenSelection::Composed(selection),"document_placement":placement})));
        effects
    }

    pub(crate) fn open_composition_in_new_surface(
        &mut self,
        surface_instance_id: SurfaceInstanceId,
        view_set_id: ViewSetId,
        binding: UiBindingAttachmentCoordinate,
        view_ref: Option<String>,
        dialog_id: String,
        dialog_binding: UiBindingAttachmentCoordinate,
    ) -> Vec<RyeOsEffect> {
        let captured = (|| -> Result<_, String> {
            if !self.composition_write_available() {
                return Err("composition is awaiting acknowledgement".into());
            }
            let surface = self
                .active_surface()
                .ok_or("source Surface is unavailable")?;
            if surface.id() != &surface_instance_id
                || surface.active_view_set().map(|set| set.id) != Some(view_set_id)
                || self.insertion_attachment_id(view_set_id)
                    != Some(binding.binding_attachment_id.as_str())
            {
                return Err("captured composition is no longer current".into());
            }
            let descriptor = self
                .binding_attachment(&binding.binding_attachment_id)
                .filter(|entry| {
                    entry.binding_generation == binding.binding_generation
                        && entry.binding_digest == binding.binding_digest
                })
                .ok_or("source binding changed")?;
            let stable_origin = |descriptor: &crate::ui::binding::UiBindingAttachment| -> Result<
                crate::surface::saved_surfaces::SavedSurfaceOrigin,
                String,
            > {
                if descriptor.presentation.definition.surface().is_none() {
                    return Err("composition origin is not an admitted Surface".into());
                }
                Ok(crate::surface::saved_surfaces::SavedSurfaceOrigin {
                    surface_ref: descriptor.root_ref.clone(),
                    project: match &descriptor.subject {
                        crate::ui::binding::UiBindingSubject::Projectless => None,
                        crate::ui::binding::UiBindingSubject::RegisteredProject { local_id } => {
                            Some(crate::surface::view_sets::ParticularProjectRef {
                                local_id: local_id.clone(),
                            })
                        }
                        _ => {
                            return Err(
                                "register project before opening a reusable composition".into()
                            );
                        }
                    },
                })
            };
            let tab_origin = stable_origin(descriptor)?;
            let surface_binding = surface.binding();
            let surface_descriptor = self
                .binding_attachment(&surface_binding.binding_attachment_id)
                .filter(|entry| {
                    entry.binding_generation == surface_binding.binding_generation
                        && entry.binding_digest == surface_binding.binding_digest
                })
                .ok_or("originating Surface binding changed")?;
            let origin = stable_origin(surface_descriptor)?;
            let template = if let Some(view_ref) = view_ref {
                let view = self
                    .binding_attachments
                    .get(&binding.binding_attachment_id)
                    .and_then(|entry| entry.views.get(&view_ref))
                    .filter(|view| view.degraded.is_none())
                    .ok_or("View leaves exact admitted closure")?;
                let name = view
                    .title
                    .clone()
                    .or_else(|| view.name.clone())
                    .unwrap_or_else(|| view_ref.clone());
                let facets = crate::ui::attachment::selection_dependencies(view)
                    .into_iter()
                    .collect::<Vec<_>>();
                SavedViewSetTemplate {
                    id: "single-view".into(),
                    name: name.clone(),
                    composition: ViewSetSeedSpec {
                        schema: VIEW_SET_COMPOSITION_SCHEMA,
                        id: "single-view".into(),
                        title: name,
                        root: Some(LayoutSeedSpec::Group {
                            label: None,
                            views: vec![crate::surface::ViewKindSpec(view_ref)],
                            active: 0,
                        }),
                    },
                    relationships: vec![SavedViewSelectionRelationship {
                        mount: SavedViewMountRef::Tile { index: 0 },
                        source: if facets.is_empty() {
                            SavedViewSelectionSource::FollowOwnSet
                        } else {
                            SavedViewSelectionSource::RequiredSubject {
                                input: "subject".into(),
                                facets,
                            }
                        },
                    }],
                }
            } else {
                let set = surface.active_view_set().ok_or("source tab absent")?;
                self.capture_view_set_template_at(
                    view_set_id,
                    "current-tab".into(),
                    set.title.clone(),
                    None,
                    &BTreeMap::from([(view_set_id, "current-tab".into())]),
                )?
            };
            let selection = crate::surface::opening::ComposedSurfaceSelection {
                origin,
                title: template.name.clone(),
                active_view_set_id: template.id.clone(),
                tabs: vec![crate::surface::opening::ComposedSurfaceTab {
                    origin: tab_origin,
                    template,
                    fresh_subjects: BTreeMap::new(),
                }],
            };
            selection.validate()?;
            let presentation = self
                .presentation
                .as_ref()
                .ok_or("presentation unavailable")?;
            let dialog = presentation
                .dialogs()
                .get(&dialog_id)
                .filter(|entry| {
                    entry.surface_owner().is_none() && presentation.dialog_available(entry)
                })
                .ok_or("shared Surface opener unavailable")?;
            let instance = dialog.view().instance_key().clone();
            let descriptor = self
                .binding_attachment_for_instance(&instance)
                .ok_or("dialog binding missing")?;
            if descriptor.binding_attachment_id != dialog_binding.binding_attachment_id
                || descriptor.binding_generation != dialog_binding.binding_generation
                || descriptor.binding_digest != dialog_binding.binding_digest
            {
                return Err("dialog binding changed".into());
            }
            let view_ref = dialog.view().view_ref().to_owned();
            let view = self
                .binding_for_instance(&instance, &view_ref)
                .ok_or("dialog View missing")?;
            if !view.affordances.iter().any(|entry| {
                entry.get("id").and_then(serde_json::Value::as_str) == Some("open-composed-surface")
            }) {
                return Err("dialog has no admitted composed opening".into());
            }
            Ok((instance, view_ref, selection))
        })();
        let Ok((instance, view_ref, selection)) = captured else {
            return Vec::new();
        };
        let mut effects = self.open_dialog(&dialog_id);
        effects.extend(self.invoke_affordance(&instance,&view_ref,"open-composed-surface",&serde_json::json!({"opening_selection":crate::surface::opening::SurfaceOpenSelection::Composed(selection)})));
        effects
    }

    pub(crate) fn open_view_in_new_tab(
        &mut self,
        surface_instance_id: SurfaceInstanceId,
        view_set_id: ViewSetId,
        binding: UiBindingAttachmentCoordinate,
        view_ref: String,
    ) -> Vec<RyeOsEffect> {
        let result = (|| -> Result<_, String> {
            let surface = self
                .active_surface()
                .ok_or("captured Surface is not active")?;
            if surface.id() != &surface_instance_id
                || surface.active_view_set().map(|set| set.id) != Some(view_set_id)
                || self.insertion_attachment_id(view_set_id)
                    != Some(binding.binding_attachment_id.as_str())
            {
                return Err("captured tab or insertion binding is no longer current".into());
            }
            let descriptor = self
                .binding_attachment(&binding.binding_attachment_id)
                .filter(|entry| {
                    entry.binding_generation == binding.binding_generation
                        && entry.binding_digest == binding.binding_digest
                })
                .ok_or("captured insertion attachment is unavailable")?;
            let view = self
                .binding_attachments
                .get(&descriptor.binding_attachment_id)
                .and_then(|entry| entry.views.get(&view_ref))
                .filter(|view| view.degraded.is_none())
                .ok_or("View is outside the current admitted insertion closure")?;
            let title = view
                .title
                .clone()
                .or_else(|| view.name.clone())
                .unwrap_or_else(|| view_ref.clone());
            let facets = crate::ui::attachment::selection_dependencies(view)
                .into_iter()
                .collect::<Vec<_>>();
            let source = if facets.is_empty() {
                SavedViewSelectionSource::FollowOwnSet
            } else {
                SavedViewSelectionSource::RequiredSubject {
                    input: "subject".into(),
                    facets,
                }
            };
            let template = SavedViewSetTemplate {
                id: "single-view".into(),
                name: title.clone(),
                composition: ViewSetSeedSpec {
                    schema: VIEW_SET_COMPOSITION_SCHEMA,
                    id: "single-view".into(),
                    title,
                    root: Some(LayoutSeedSpec::Group {
                        label: None,
                        views: vec![crate::surface::ViewKindSpec(view_ref)],
                        active: 0,
                    }),
                },
                relationships: vec![SavedViewSelectionRelationship {
                    mount: SavedViewMountRef::Tile { index: 0 },
                    source,
                }],
            };
            self.open_saved_view_set_template_with_policy_at(
                &template,
                &binding.binding_attachment_id,
                &BTreeMap::new(),
                &BTreeMap::new(),
                RequiredSubjectMountPolicy::AllowUnresolved,
                &PresentationInsertionDestination {
                    surface_instance_id,
                    view_set_id,
                },
            )
        })();
        match result {
            Ok(effects) => effects,
            Err(reason) => {
                self.notice(
                    format!("Cannot open View in a new tab: {reason}"),
                    RyeOsTone::Warn,
                );
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::reducer::test_support::*;
    use serde_json::json;

    fn fixture() -> RyeOsCore {
        core(session_with_surface(
            json!({"name":"one","tiles":["view:test/plain"],"views":{
                "view:test/plain":{"widget":"rows"},
                "view:test/subject":{"title":"Subject","widget":"rows","sources":{"default":{"ref":"service:test/read","params":{"id":"@facet:selection.work.id"}}}}
            }}),
        ))
    }

    fn composed_fixture() -> RyeOsCore {
        composed_fixture_with_surface(json!({"name":"Source","tiles":["view:test/plain"],"views":{
                "view:test/plain":{"title":"Plain","widget":"rows","sources":{"default":{"ref":"service:test/read","params":{}}}}
        }}))
    }

    fn composed_fixture_with_surface(surface: serde_json::Value) -> RyeOsCore {
        let mut session = session_with_surface(surface);
        for attachment in &mut session.binding_attachments {
            attachment.subject = crate::ui::binding::UiBindingSubject::Projectless;
            attachment.project_path = None;
            if attachment.binding_attachment_id == session.root_attachment_id {
                attachment.presentation.views.insert("view:ryeos/launcher".into(), serde_json::from_value(json!({
                    "widget":"rows","affordances":[{"id":"open-composed-surface","presentation_operation":"surface_opening",
                        "invoke":{"plane":"rye","ref":"service:ui/session/surfaces/open","args":{
                            "selection":"{record.opening_selection}","placement":{"kind":"new_surface"},"creation_request_id":"{record.creation_request_id}"}}},
                        {"id":"open-composed-document","presentation_operation":"surface_opening","invoke":{"plane":"rye","ref":"service:ui/session/surfaces/open","args":{
                            "selection":"{record.opening_selection}","placement":"{record.document_placement}","creation_request_id":"{record.creation_request_id}"}}},
                        {"id":"duplicate-particular-surface","presentation_operation":"resume_particular_surface","invoke":{"plane":"rye","ref":"service:ui/ryeos-ui/surfaces/resume","args":{
                            "id":"{record.id}","expected_revision":"{record.expected_revision}","view_set_expected_revision":"{record.view_set_expected_revision}","creation_request_id":"{record.creation_request_id}"}}}]
                })).unwrap());
            }
        }
        core(session)
    }

    #[test]
    fn particular_surface_explicit_duplicates_have_independent_mounts_and_drafts_after_two_acks() {
        use crate::ui::{
            binding::*,
            effect::{RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind},
            model::{InputBufferKey, RyeOsInputState},
        };
        let mut core = composed_fixture_with_surface(
            json!({"name":"Saved origin","tiles":["view:test/plain"],"views":{"view:test/plain":{"widget":"text","input":{"id":"draft","local_filter":true}}}}),
        );
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let origin_template = core
            .binding_attachment(
                &core
                    .active_surface()
                    .unwrap()
                    .binding()
                    .binding_attachment_id,
            )
            .unwrap()
            .clone();
        let initial_count = core.presentation.as_ref().unwrap().surfaces().len();
        let mut original = None;
        let mut operation_ids = Vec::new();
        let mut mount_ids = Vec::new();
        for ordinal in 2..=3 {
            core.open_dialog("launcher");
            settle_pending_prefix(&mut core, &producer);
            let shared = core.presentation.as_ref().unwrap().dialogs()["launcher"]
                .view()
                .instance_key()
                .clone();
            let before_count = core.presentation.as_ref().unwrap().surfaces().len();
            let effects = core.invoke_affordance(
                &shared,
                "view:ryeos/launcher",
                "duplicate-particular-surface",
                &json!({"id":"saved","expected_revision":4,"view_set_expected_revision":6}),
            );
            assert!(!effects.iter().any(|effect| matches!(
                effect.kind,
                RyeOsEffectKind::InvokeBinding { .. }
                    | RyeOsEffectKind::InvokePresentationOperation { .. }
            )));
            let prepared = pending_prefix(&core)
                .into_iter()
                .find_map(|event| match event.kind {
                    crate::ui::seat::SeatEventKind::PresentationOperationPrepared(operation) => {
                        Some(operation)
                    }
                    _ => None,
                })
                .unwrap();
            operation_ids.push(prepared.operation_id.clone());
            assert_eq!(prepared.admission().unwrap().purpose,crate::ui::admission_operation::UiPresentationOperationKind::ResumeParticularSurface);
            let effects = core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
            let invocation = effects
                .iter()
                .find(|effect| {
                    matches!(
                        effect.kind,
                        RyeOsEffectKind::InvokePresentationOperation { .. }
                    )
                })
                .unwrap();
            let RyeOsEffectKind::InvokePresentationOperation { operation } = &invocation.kind
            else {
                unreachable!();
            };
            assert_eq!(operation, &prepared);
            let request = &operation.admission().unwrap().request;
            let UiBindingPayload::Selection { record } = &request.payload else {
                panic!("exact duplicate selection payload");
            };
            assert_eq!(record["id"], "saved");
            assert_eq!(record["expected_revision"], 4);
            assert_eq!(record["view_set_expected_revision"], 6);
            assert_eq!(record["creation_request_id"], prepared.operation_id);
            assert!(
                matches!(&request.coordinate,UiBindingCoordinate::Affordance{affordance_id,..} if affordance_id=="duplicate-particular-surface")
            );
            assert_eq!(
                core.presentation.as_ref().unwrap().surfaces().len(),
                before_count
            );
            let mut origin = origin_template.clone();
            origin.binding_attachment_id = format!("copy-origin-{ordinal}");
            origin.binding_generation += ordinal;
            let mut tab = origin.clone();
            tab.binding_attachment_id = format!("copy-tab-{ordinal}");
            tab.binding_generation += 10;
            let coordinate = |entry: &UiBindingAttachment| UiBindingAttachmentCoordinate {
                binding_attachment_id: entry.binding_attachment_id.clone(),
                binding_generation: entry.binding_generation,
                binding_digest: entry.binding_digest.clone(),
            };
            let resolved=serde_json::from_value(json!({"id":"saved-tab","name":"Saved tab","composition":{"schema":2,"id":"saved-tab","title":"Saved tab","root":{"type":"group","views":["view:test/plain"],"active":0}},"relationships":[{"mount":{"kind":"tile","index":0},"source":{"mode":"follow_own_set"}}],"resolved_selection_work":{"thread":"T-saved-head","chain_root":"T-saved-root"}})).unwrap();
            let saved = UiSavedSurfaceComposition {
                provenance: UiSurfaceCompositionProvenance::Library {
                    saved_id: "saved".into(),
                    library_revision: 4,
                    record_digest: "b".repeat(64),
                },
                active_view_set_id: "saved-tab".into(),
                body: UiSavedSurfaceCompositionBody::Particular {
                    tabs: vec![UiSavedSurfaceTab {
                        saved_set_id: "saved-tab".into(),
                        binding: coordinate(&tab),
                        resolved,
                    }],
                },
            };
            let manifest = crate::ui::composition_identity::UiSurfaceMountManifest::allocate(
                crate::ids::PresentationId::try_new(producer.presentation_id.clone()).unwrap(),
                SurfaceInstanceId::try_new(format!("00000000-0000-4000-8000-{ordinal:012}"))
                    .unwrap(),
                core.acknowledged_composition_replay()
                    .unwrap()
                    .next_mount_id,
                saved.mount_seeds(),
                Some("saved-tab"),
            )
            .unwrap();
            let member = UiSurfaceMembership {
                surface_instance_id: manifest.surface_instance_id.as_str().into(),
                ordinal,
                title: "Saved origin".into(),
                binding: coordinate(&origin),
                mount_manifest: manifest,
                saved_surface: Some(saved.clone()),
            };
            let data = json!({"particular_surface":saved,"particular_selection":{"kind":"surface","id":"saved","expected_revision":4},"ui_transition":{"kind":"resume_particular_surface","presentation_id":producer.presentation_id,"creation_request_id":prepared.operation_id,"surface":member,"attachment":origin,"tab_attachments":[tab]}});
            let effects = core.apply_effect_result(RyeOsEffectResult {
                id: invocation.id,
                ok: true,
                kind: RyeOsEffectResultKind::BindingInvoked,
                data: Some(data),
                error: None,
            });
            assert!(effects.is_empty());
            assert_eq!(
                core.presentation.as_ref().unwrap().surfaces().len(),
                before_count,
                "completion without ACK never allocates local mounts"
            );
            core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
            let set = core.active_view_set().unwrap();
            let set_id = set.id;
            let tile = set.tiles.values().next().unwrap();
            let instance = tile.instance_key.clone();
            mount_ids.push(instance.clone());
            let key = InputBufferKey::new(
                instance.clone(),
                "view:test/plain",
                "draft",
                core.binding_attachment_for_instance(&instance).unwrap(),
            )
            .storage_key();
            assert!(
                core.input_buffers
                    .get(&key)
                    .is_none_or(|draft| draft.text.is_empty()),
                "new exact saved instance does not copy existing drafts"
            );
            assert_eq!(
                core.selection_value_in_view_set(set_id, "selection.work")
                    .unwrap()["chain_root"],
                "T-saved-root"
            );
            let active_origin = core.active_surface().unwrap().binding().clone();
            assert_eq!(
                core.binding_attachment(&active_origin.binding_attachment_id)
                    .unwrap()
                    .root_ref,
                origin_template.root_ref
            );
            if let Some((original_key, original_set, original_binding)) = &original {
                assert_eq!(core.input_buffers[original_key].text, "first copy draft");
                assert_eq!(
                    core.selection_value_in_view_set(*original_set, "selection.work")
                        .unwrap()["chain_root"],
                    "T-saved-root"
                );
                assert!(
                    core.presentation
                        .as_ref()
                        .unwrap()
                        .surfaces()
                        .values()
                        .any(|surface| surface.binding() == original_binding)
                );
            } else {
                core.input_buffers.insert(
                    key.clone(),
                    RyeOsInputState {
                        text: "first copy draft".into(),
                        ..Default::default()
                    },
                );
                original = Some((key, set_id, active_origin));
            }
        }
        assert_ne!(operation_ids[0], operation_ids[1]);
        assert_ne!(mount_ids[0], mount_ids[1]);
        assert_eq!(
            core.presentation.as_ref().unwrap().surfaces().len(),
            initial_count + 2
        );
    }

    #[test]
    fn selected_document_waits_for_two_acks_and_never_mounts_in_parent() {
        use crate::ui::{
            binding::*,
            effect::{RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind},
            presentation_initialization::*,
        };
        let mut core = composed_fixture_with_surface(json!({"name":"Selected","view_sets":[
            {"schema":2,"id":"first","title":"First","root":{"type":"group","views":["view:test/plain"],"active":0}},
            {"schema":2,"id":"second","title":"Second","root":{"type":"group","views":["view:test/plain"],"active":0}}
        ],"views":{"view:test/plain":{"widget":"rows","sources":{"default":{"ref":"service:test/read","params":{}}}}}}));
        core.supports_independent_documents = true;
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let before = core.acknowledged_composition_replay().unwrap();
        let surface = core.active_surface().unwrap();
        let id = surface.id().clone();
        let binding = surface.binding().clone();
        let mut origin = core
            .binding_attachment(&binding.binding_attachment_id)
            .unwrap()
            .clone();
        let root = core.presentation.as_ref().unwrap().root_binding().clone();
        let root_descriptor = core
            .binding_attachment(&root.binding_attachment_id)
            .unwrap()
            .clone();
        let dialog_binding = core.presentation.as_ref().unwrap().dialogs()["launcher"]
            .view()
            .binding()
            .clone();
        let effects =
            core.open_surface_in_new_presentation(id, binding, "launcher".into(), dialog_binding);
        assert!(!effects.iter().any(|effect| matches!(
            effect.kind,
            RyeOsEffectKind::InvokeBinding { .. }
                | RyeOsEffectKind::InvokePresentationOperation { .. }
                | RyeOsEffectKind::ShowPreparedPresentationDocument { .. }
        )));
        let prepared = pending_prefix(&core)
            .into_iter()
            .find_map(|event| match event.kind {
                crate::ui::seat::SeatEventKind::PresentationOperationPrepared(operation) => {
                    Some(operation)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(
            prepared.admission().unwrap().purpose,
            crate::ui::admission_operation::UiPresentationOperationKind::OpenPresentationDocument
        );
        let target = prepared
            .admission()
            .unwrap()
            .surface_open_target
            .as_ref()
            .unwrap();
        let crate::surface::opening::SurfaceOpenSelection::Composed(selection) = &target.selection
        else {
            panic!("portable exact selection");
        };
        assert_eq!(selection.tabs.len(), 2);
        assert!(
            selection
                .tabs
                .iter()
                .all(|tab| tab.fresh_subjects.is_empty())
        );
        let effects = core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
        let invocation = effects
            .iter()
            .find(|effect| {
                matches!(
                    effect.kind,
                    RyeOsEffectKind::InvokePresentationOperation { .. }
                )
            })
            .unwrap();
        let RyeOsEffectKind::InvokePresentationOperation { operation } = &invocation.kind else {
            unreachable!();
        };
        assert_eq!(operation, &prepared);
        assert!(!effects.iter().any(|effect| matches!(
            effect.kind,
            RyeOsEffectKind::ShowPreparedPresentationDocument { .. }
        )));
        origin.binding_attachment_id = "document-origin".into();
        origin.binding_generation += 10;
        let tab_descriptors = selection
            .tabs
            .iter()
            .enumerate()
            .map(|(index, _)| {
                let mut tab = origin.clone();
                tab.binding_attachment_id = format!("document-tab-{index}");
                tab.binding_generation += 1 + index as u64;
                tab
            })
            .collect::<Vec<_>>();
        let coordinate = |entry: &UiBindingAttachment| UiBindingAttachmentCoordinate {
            binding_attachment_id: entry.binding_attachment_id.clone(),
            binding_generation: entry.binding_generation,
            binding_digest: entry.binding_digest.clone(),
        };
        let composition = UiSavedSurfaceComposition {
            provenance: UiSurfaceCompositionProvenance::Composed {
                composition: selection.clone(),
            },
            active_view_set_id: selection.active_view_set_id.clone(),
            body: UiSavedSurfaceCompositionBody::Composed {
                tabs: selection
                    .tabs
                    .iter()
                    .zip(&tab_descriptors)
                    .map(|(selected, tab)| UiReusableSurfaceTab {
                        saved_set_id: selected.template.id.clone(),
                        binding: coordinate(tab),
                        origin: selected.origin.clone(),
                        template: selected.template.clone(),
                        fresh_subjects: selected.fresh_subjects.clone(),
                    })
                    .collect(),
            },
        };
        let child_id = "88888888-8888-4888-8888-888888888888";
        let manifest = crate::ui::composition_identity::UiSurfaceMountManifest::allocate(
            crate::ids::PresentationId::try_new(child_id).unwrap(),
            SurfaceInstanceId::try_new("99999999-9999-4999-8999-999999999999").unwrap(),
            1,
            composition.mount_seeds(),
            Some(&composition.active_view_set_id),
        )
        .unwrap();
        let opening = crate::surface::opening::SurfaceOpenRequest {
            creation_request_id: prepared.operation_id.clone(),
            selection: target.selection.clone(),
            placement: target.placement.clone(),
        };
        let mut descriptors = vec![root_descriptor, origin.clone()];
        descriptors.extend(tab_descriptors);
        descriptors.sort_by(|a, b| a.binding_attachment_id.cmp(&b.binding_attachment_id));
        let initialization = UiPresentationInitialization {
            schema: 2,
            selection: UiPresentationInitialSelection::SelectedSurface {
                opening: opening.clone(),
            },
            presentation_id: child_id.into(),
            creation_request_id: prepared.operation_id.clone(),
            root_binding: root.clone(),
            next_mount_id: manifest.next_mount_id,
            surfaces: vec![UiSurfaceMembership {
                surface_instance_id: manifest.surface_instance_id.as_str().into(),
                ordinal: 1,
                title: selection.title.clone(),
                binding: coordinate(&origin),
                mount_manifest: manifest,
                saved_surface: Some(composition),
            }],
            binding_attachments: descriptors,
        };
        initialization.validate().unwrap();
        let receipt = UiPreparedPresentationDocumentTransition {
            kind: UiPreparedPresentationDocumentKind::OpenPresentationDocument,
            presentation_id: producer.presentation_id.clone(),
            creation_request_id: prepared.operation_id.clone(),
            opening,
            prepared: UiPreparedPresentationDocument {
                presentation_id: child_id.into(),
                thread_id: "T-88888888-8888-4888-8888-888888888888".into(),
                chain_root_id: "T-88888888-8888-4888-8888-888888888888".into(),
                root_binding: root,
                initialization: initialization.clone(),
            },
        };
        receipt.validate().unwrap();
        let result = json!({"ui_transition":receipt});
        let effects = core.apply_effect_result(RyeOsEffectResult {
            id: invocation.id,
            ok: true,
            kind: RyeOsEffectResultKind::BindingInvoked,
            data: Some(result),
            error: None,
        });
        assert!(
            effects.is_empty(),
            "returned preparation is not a retained completion ACK"
        );
        let effects = core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
        assert_eq!(effects.iter().filter(|effect|matches!(&effect.kind,RyeOsEffectKind::ShowPreparedPresentationDocument{presentation_id} if presentation_id==child_id)).count(),1);
        assert!(
            !effects.iter().any(|effect| matches!(
                effect.kind,
                RyeOsEffectKind::FetchSource { .. }
                    | RyeOsEffectKind::OpenNewPresentationDocument { .. }
            )),
            "parent neither mounts child sources nor prepares again"
        );
        let after = core.acknowledged_composition_replay().unwrap();
        assert_eq!(before.next_mount_id, after.next_mount_id);
        assert_eq!(
            format!("{:?}", before.composition),
            format!("{:?}", after.composition)
        );
        assert_eq!(before.binding_descriptors(), after.binding_descriptors());
        let events = &core.seat.events()[..core.presentation_operations.acknowledged_prefix_len];
        let crate::ui::seat::SeatEventKind::PresentationInitialized(initial) = &events[0].kind
        else {
            panic!("retained initialization");
        };
        let cold = crate::ui::composition_replay::UiCompositionReplay::from_history(
            &initial.initialization,
            events,
        )
        .unwrap();
        assert_eq!(
            format!("{:?}", cold.composition),
            format!("{:?}", before.composition)
        );
        assert_eq!(cold.next_mount_id, before.next_mount_id);
        let child_event = crate::ui::seat::SeatEvent {
            seq: 0,
            producer_engine_seq: None,
            kind: crate::ui::seat::SeatEventKind::PresentationInitialized(
                UiPresentationInitializationRecord {
                    operation_id: prepared.operation_id,
                    initialization: initialization.clone(),
                },
            ),
        };
        let child = crate::ui::composition_replay::UiCompositionReplay::from_history(
            &initialization,
            &[child_event],
        )
        .unwrap();
        assert_eq!(
            child
                .composition
                .active_surface()
                .unwrap()
                .view_sets()
                .len(),
            2
        );
    }

    #[test]
    fn composed_current_tab_preserves_nested_layout_without_copying_ambient_subject() {
        let mut core = composed_fixture_with_surface(json!({"name":"Nested","view_sets":[{
            "schema":2,"id":"nested","title":"Nested","root":{"type":"split","axis":"horizontal","ratio":0.6,
                "first":{"type":"group","views":["view:test/plain","view:test/other"],"active":1},
                "second":{"type":"group","views":["view:test/plain"],"active":0}}
        }],"views":{"view:test/plain":{"widget":"rows"},"view:test/other":{"widget":"rows"}}}));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let surface = core.active_surface().unwrap();
        let id = surface.id().clone();
        let tab = surface.active_view_set().unwrap().id;
        let binding = surface.binding().clone();
        let original_layout = surface.active_view_set().unwrap().root.clone();
        let dialog_binding = core.presentation.as_ref().unwrap().dialogs()["launcher"]
            .view()
            .binding()
            .clone();
        core.seat.append_facet(
            crate::ui::seat::selection_storage_key(tab, "selection.work").unwrap(),
            json!({"id":"old-work"}),
        );
        settle_pending_prefix(&mut core, &producer);
        core.open_composition_in_new_surface(
            id,
            tab,
            binding,
            None,
            "launcher".into(),
            dialog_binding,
        );
        let prepared = pending_prefix(&core)
            .into_iter()
            .find_map(|event| match event.kind {
                crate::ui::seat::SeatEventKind::PresentationOperationPrepared(operation) => {
                    Some(operation)
                }
                _ => None,
            })
            .expect("nested tab opening preparation");
        let target = prepared
            .admission()
            .unwrap()
            .surface_open_target
            .as_ref()
            .unwrap();
        let crate::surface::opening::SurfaceOpenSelection::Composed(selection) = &target.selection
        else {
            panic!("composed selection")
        };
        let Some(LayoutSeedSpec::Split { first, second, .. }) =
            &selection.tabs[0].template.composition.root
        else {
            panic!("nested split must survive capture")
        };
        assert!(
            matches!(first.as_ref(),LayoutSeedSpec::Group {views,active:1,..} if views.len()==2)
        );
        assert!(matches!(second.as_ref(),LayoutSeedSpec::Group {views,..} if views.len()==1));
        assert!(selection.tabs[0].fresh_subjects.is_empty());
        assert_eq!(
            core.active_surface()
                .unwrap()
                .active_view_set()
                .unwrap()
                .root,
            original_layout
        );
        assert_eq!(core.presentation.as_ref().unwrap().surfaces().len(), 1);
    }

    #[test]
    fn composed_capture_keeps_surface_origin_distinct_from_tab_origin() {
        let mut core = composed_fixture();
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let surface = core.active_surface().unwrap();
        let surface_id = surface.id().clone();
        let tab = surface.active_view_set().unwrap().id;
        let mut descriptor = core
            .binding_attachment(&surface.binding().binding_attachment_id)
            .unwrap()
            .clone();
        descriptor.binding_attachment_id = "different-tab-origin".into();
        descriptor.binding_generation += 5;
        descriptor.root_ref = "surface:test/other-origin".into();
        let binding = UiBindingAttachmentCoordinate {
            binding_attachment_id: descriptor.binding_attachment_id.clone(),
            binding_generation: descriptor.binding_generation,
            binding_digest: descriptor.binding_digest.clone(),
        };
        // Admitted-descriptor capture fixture, not a compiler/membership grant.
        core.binding_attachments.insert(
            descriptor.binding_attachment_id.clone(),
            crate::ui::binding_context::RetainedUiBindingAttachment::from_descriptor(descriptor)
                .unwrap(),
        );
        assert!(core.set_view_set_insertion_attachment(tab, &binding.binding_attachment_id));
        let dialog_binding = core.presentation.as_ref().unwrap().dialogs()["launcher"]
            .view()
            .binding()
            .clone();
        core.open_composition_in_new_surface(
            surface_id,
            tab,
            binding,
            Some("view:test/plain".into()),
            "launcher".into(),
            dialog_binding,
        );
        let prepared = pending_prefix(&core)
            .into_iter()
            .find_map(|event| match event.kind {
                crate::ui::seat::SeatEventKind::PresentationOperationPrepared(operation) => {
                    Some(operation)
                }
                _ => None,
            })
            .expect("mixed-origin composition captured");
        let target = prepared
            .admission()
            .unwrap()
            .surface_open_target
            .as_ref()
            .unwrap();
        let crate::surface::opening::SurfaceOpenSelection::Composed(selection) = &target.selection
        else {
            panic!("composed selection")
        };
        assert_eq!(selection.origin.surface_ref, "surface:ryeos/ryeos/base");
        assert_eq!(
            selection.tabs[0].origin.surface_ref,
            "surface:test/other-origin"
        );
    }

    #[test]
    fn composed_single_view_captures_origin_and_waits_for_preparation_ack() {
        assert_composed_opening_ack_and_cold_fold(
            composed_fixture(),
            Some("view:test/plain".into()),
            1,
        );
    }

    #[test]
    fn composed_nested_tab_opens_after_completion_ack_and_cold_folds() {
        let core = composed_fixture_with_surface(json!({"name":"Nested","view_sets":[{
            "schema":2,"id":"nested","title":"Nested","root":{"type":"split","axis":"horizontal","ratio":0.6,
                "first":{"type":"group","views":["view:test/plain","view:test/other"],"active":1},
                "second":{"type":"group","views":["view:test/plain"],"active":0}}
        }],"views":{
            "view:test/plain":{"widget":"rows","sources":{"default":{"ref":"service:test/read","params":{}}}},
            "view:test/other":{"widget":"rows","sources":{"default":{"ref":"service:test/read","params":{}}}}
        }}));
        assert_composed_opening_ack_and_cold_fold(core, None, 3);
    }

    fn assert_composed_opening_ack_and_cold_fold(
        mut core: RyeOsCore,
        view_ref: Option<String>,
        expected_tiles: usize,
    ) {
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let surface = core.active_surface().unwrap();
        let id = surface.id().clone();
        let tab = surface.active_view_set().unwrap().id;
        let binding = surface.binding().clone();
        let dialog_binding = core.presentation.as_ref().unwrap().dialogs()["launcher"]
            .view()
            .binding()
            .clone();
        let before = core.presentation.as_ref().unwrap().surfaces().len();
        let effects = core.open_composition_in_new_surface(
            id,
            tab,
            binding,
            view_ref,
            "launcher".into(),
            dialog_binding,
        );
        assert!(effects.iter().all(|effect| !matches!(
            effect.kind,
            RyeOsEffectKind::InvokeBinding { .. }
                | RyeOsEffectKind::InvokePresentationOperation { .. }
        )));
        assert_eq!(core.presentation.as_ref().unwrap().surfaces().len(), before);
        let prepared = pending_prefix(&core)
            .into_iter()
            .find_map(|event| match event.kind {
                crate::ui::seat::SeatEventKind::PresentationOperationPrepared(operation) => {
                    Some(operation)
                }
                _ => None,
            })
            .expect("composed opening is retained before dispatch");
        let target = prepared
            .admission()
            .unwrap()
            .surface_open_target
            .as_ref()
            .unwrap();
        let crate::surface::opening::SurfaceOpenSelection::Composed(selection) = &target.selection
        else {
            panic!("typed composed selection required")
        };
        assert_eq!(selection.origin.surface_ref, "surface:ryeos/ryeos/base");
        assert!(selection.origin.project.is_none());
        assert!(selection.tabs[0].fresh_subjects.is_empty());
        let effects = core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
        let invocation = effects
            .iter()
            .find(|effect| {
                matches!(
                    effect.kind,
                    RyeOsEffectKind::InvokePresentationOperation { .. }
                )
            })
            .unwrap();
        let RyeOsEffectKind::InvokePresentationOperation { operation } = &invocation.kind else {
            unreachable!();
        };
        assert_eq!(operation, &prepared);
        let mut origin = core
            .binding_attachment(
                &core
                    .active_surface()
                    .unwrap()
                    .binding()
                    .binding_attachment_id,
            )
            .unwrap()
            .clone();
        origin.binding_attachment_id = "composed-origin".into();
        origin.binding_generation += 10;
        let mut tab_attachment = origin.clone();
        tab_attachment.binding_attachment_id = "composed-tab".into();
        tab_attachment.binding_generation += 1;
        let coordinate =
            |descriptor: &crate::ui::binding::UiBindingAttachment| UiBindingAttachmentCoordinate {
                binding_attachment_id: descriptor.binding_attachment_id.clone(),
                binding_generation: descriptor.binding_generation,
                binding_digest: descriptor.binding_digest.clone(),
            };
        let composition = crate::ui::binding::UiSavedSurfaceComposition {
            provenance: crate::ui::binding::UiSurfaceCompositionProvenance::Composed {
                composition: selection.clone(),
            },
            active_view_set_id: selection.active_view_set_id.clone(),
            body: crate::ui::binding::UiSavedSurfaceCompositionBody::Composed {
                tabs: selection
                    .tabs
                    .iter()
                    .map(|tab| crate::ui::binding::UiReusableSurfaceTab {
                        saved_set_id: tab.template.id.clone(),
                        binding: coordinate(&tab_attachment),
                        origin: tab.origin.clone(),
                        template: tab.template.clone(),
                        fresh_subjects: tab.fresh_subjects.clone(),
                    })
                    .collect(),
            },
        };
        let manifest = crate::ui::composition_identity::UiSurfaceMountManifest::allocate(
            crate::ids::PresentationId::try_new(producer.presentation_id.clone()).unwrap(),
            SurfaceInstanceId::try_new("22222222-2222-4222-8222-222222222222").unwrap(),
            core.acknowledged_composition_replay()
                .unwrap()
                .next_mount_id,
            composition.mount_seeds(),
            Some(&composition.active_view_set_id),
        )
        .unwrap();
        let member = crate::ui::binding::UiSurfaceMembership {
            surface_instance_id: manifest.surface_instance_id.as_str().into(),
            ordinal: 2,
            title: selection.title.clone(),
            binding: coordinate(&origin),
            mount_manifest: manifest,
            saved_surface: Some(composition),
        };
        let result = json!({"ui_transition":{"kind":"open_surface","presentation_id":producer.presentation_id,
            "creation_request_id":prepared.operation_id,"opening":{"creation_request_id":prepared.operation_id,"selection":target.selection,"placement":target.placement},
            "surface":member,"attachment":origin,"tab_attachments":[tab_attachment]}});
        let effects = core.apply_effect_result(RyeOsEffectResult {
            id: invocation.id,
            ok: true,
            kind: RyeOsEffectResultKind::BindingInvoked,
            data: Some(result),
            error: None,
        });
        assert!(effects.is_empty(), "no source work before completion ACK");
        assert_eq!(core.presentation.as_ref().unwrap().surfaces().len(), before);
        assert!(pending_prefix(&core).iter().any(|event| matches!(
            event.kind,
            crate::ui::seat::SeatEventKind::PresentationOperationCompleted(_)
        )));
        let published_effects = core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
        assert_eq!(
            core.presentation.as_ref().unwrap().surfaces().len(),
            before + 1
        );
        assert_eq!(core.active_surface().unwrap().view_sets().len(), 1);
        assert_eq!(core.active_view_set().unwrap().tiles.len(), expected_tiles);
        if expected_tiles == 3 {
            let crate::layout::LayoutTree::Split { first, second, .. } =
                core.active_view_set().unwrap().root.as_ref().unwrap()
            else {
                panic!("nested split must survive admission");
            };
            assert_eq!(first.tile_ids().len(), 2);
            assert_eq!(second.tile_ids().len(), 1);
            assert!(
                matches!(first.as_ref(), crate::layout::LayoutTree::Group { tabs, active, .. }
                if *active == tabs[1])
            );
        }
        let instances = core
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .map(|tile| tile.instance_key.clone())
            .collect::<Vec<_>>();
        assert!(
            published_effects.iter().any(|effect| matches!(&effect.kind,
            RyeOsEffectKind::FetchSource {tile_id,request,..}
            if crate::ui::source_key::RyeOsSourceInstanceKey::decode(tile_id).is_some_and(|key| instances.contains(&key.view_instance))
                && request.binding_attachment_id == "composed-tab")),
            "source starts only under the newly acknowledged mount and exact admitted tab binding"
        );
        // Reconstruct from the immutable initialization and retained ACKed
        // events, not the live Core composition or a synthetic saved-in-init seed.
        let events = &core.seat.events()[..core.presentation_operations.acknowledged_prefix_len];
        let crate::ui::seat::SeatEventKind::PresentationInitialized(initial) = &events[0].kind
        else {
            panic!("fixture starts with retained initialization");
        };
        let cold = crate::ui::composition_replay::UiCompositionReplay::from_history(
            &initial.initialization,
            events,
        )
        .unwrap();
        let live = core.acknowledged_composition_replay().unwrap();
        // ViewSet tiles use HashMap storage. Compare structured values rather
        // than Debug iteration order from independently reconstructed maps.
        assert_eq!(cold.composition.id(), live.composition.id());
        assert_eq!(
            cold.composition.root_binding(),
            live.composition.root_binding()
        );
        assert_eq!(
            cold.composition.active_surface().map(|s| s.id()),
            live.composition.active_surface().map(|s| s.id())
        );
        assert_eq!(
            cold.composition.surfaces().keys().collect::<Vec<_>>(),
            live.composition.surfaces().keys().collect::<Vec<_>>()
        );
        for (id, surface) in cold.composition.surfaces() {
            let other = &live.composition.surfaces()[id];
            assert_eq!(surface.title(), other.title());
            assert_eq!(surface.ordinal(), other.ordinal());
            assert_eq!(surface.binding(), other.binding());
            assert_eq!(
                surface.active_view_set().map(|set| set.id),
                other.active_view_set().map(|set| set.id)
            );
            assert_eq!(
                serde_json::to_value(surface.view_sets()).unwrap(),
                serde_json::to_value(other.view_sets()).unwrap()
            );
            for set in surface.view_sets() {
                for tile in set.tiles.values() {
                    assert_eq!(
                        cold.composition
                            .mount(&tile.instance_key)
                            .unwrap()
                            .binding(),
                        live.composition
                            .mount(&tile.instance_key)
                            .unwrap()
                            .binding()
                    );
                }
            }
        }
        assert_eq!(
            format!("{:?}", cold.composition.docks()),
            format!("{:?}", live.composition.docks())
        );
        assert_eq!(
            format!("{:?}", cold.composition.dialogs()),
            format!("{:?}", live.composition.dialogs())
        );
        assert_eq!(cold.subjects, live.subjects);
        assert_eq!(cold.next_mount_id, live.next_mount_id);
        assert_eq!(cold.binding_descriptors(), live.binding_descriptors());
    }

    #[test]
    fn one_view_new_tab_waits_for_two_acks_and_does_not_copy_ambient_subject() {
        let mut core = fixture();
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let surface = core.active_surface().unwrap();
        let id = surface.id().clone();
        let binding = surface.binding().clone();
        let tab = surface.active_view_set().unwrap().id;
        core.seat.append_facet(
            crate::ui::seat::selection_storage_key(tab, "selection.work").unwrap(),
            json!({"id":"old-work"}),
        );
        settle_pending_prefix(&mut core, &producer);
        assert!(
            core.open_view_in_new_tab(id, tab, binding, "view:test/subject".into())
                .is_empty()
        );
        assert_eq!(core.active_surface().unwrap().view_sets().len(), 1);
        core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
        assert_eq!(core.active_surface().unwrap().view_sets().len(), 1);
        core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
        let opened = core.active_view_set().unwrap();
        assert_ne!(opened.id, tab);
        assert_eq!(opened.tiles.len(), 1);
        let key = &opened.tiles.values().next().unwrap().instance_key;
        assert!(core.instance_has_unresolved_required_subject(key));
        assert!(
            core.selection_value_in_view_set(opened.id, "selection.work")
                .is_none()
        );
    }

    #[test]
    fn one_view_new_tab_refuses_stale_destination_binding_and_foreign_view() {
        for failure in ["tab", "surface", "generation", "view"] {
            let mut core = fixture();
            let producer = claim_fixture_producer(&mut core);
            settle_pending_prefix(&mut core, &producer);
            let surface = core.active_surface().unwrap();
            let mut id = surface.id().clone();
            let mut tab = surface.active_view_set().unwrap().id;
            let mut binding = surface.binding().clone();
            let mut view = "view:test/plain".to_owned();
            match failure {
                "tab" => tab = ViewSetId::new(u64::MAX),
                "surface" => id = SurfaceInstanceId::try_new("foreign-surface").unwrap(),
                "generation" => binding.binding_generation += 1,
                _ => view = "view:foreign/view".into(),
            }
            let before = core.seat.events().len();
            assert!(core.open_view_in_new_tab(id, tab, binding, view).is_empty());
            assert_eq!(core.seat.events().len(), before, "{failure}");
            assert_eq!(core.active_surface().unwrap().view_sets().len(), 1);
        }
    }
}
