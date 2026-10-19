//! Local composition operations. Persistence always uses the signed companion
//! affordance; a client-produced template never supplies execution authority.

use super::effect::RyeOsEffect;
use super::model::RyeOsCore;
use super::view_model::RyeOsTone;
use crate::ids::RyeOsViewInstanceKey;
use crate::surface::view_sets::{
    ParticularProjectRef, ParticularViewSetContext, ParticularViewSetResume, ParticularWorkRef,
    SavedViewSelectionSource, SavedViewSetTemplate, validate_particular_view_set_resumes,
    validate_saved_view_set_templates,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveContext {
    expected_revision: u64,
    saved_view_sets: Vec<SavedViewSetTemplate>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ParticularSaveContext {
    expected_revision: u64,
    particular_view_sets: Vec<ParticularViewSetResume>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SurfaceSaveContext {
    expected_revision: u64,
    reusable_surfaces: Vec<crate::surface::saved_surfaces::ReusableSurfaceRecord>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ParticularSurfaceSaveContext {
    expected_revision: u64,
    particular_surfaces: Vec<crate::surface::saved_surfaces::ParticularSurfaceRecord>,
    view_set_expected_revision: u64,
    particular_view_sets: Vec<ParticularViewSetResume>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::binding::{UiBindingCoordinate, UiBindingPayload};
    use crate::ui::effect::RyeOsEffectKind;
    use crate::ui::model::RyeOsCore;
    use crate::ui::reducer::test_support::core as test_support_core;
    use crate::ui::reducer::test_support::session_with_surface;
    use crate::ui::reducer::test_support::{claim_fixture_producer, settle_pending_prefix};

    fn core() -> RyeOsCore {
        let session = session_with_surface(json!({
            "name": "test",
            "tiles": ["view:test/library"],
            "views": {
                "view:test/library": {
                    "widget": "rows",
                    "affordances": [{
                        "id": "persist", "producer": "selection",
                        "invoke": { "plane": "rye", "ref": "service:test/persist", "args": {
                            "view_set_library": {
                                "expected_revision": "{record.expected_revision}",
                                "saved_view_sets": "{record.saved_view_sets}"
                            }
                        } }
                    }]
                }
            }
        }));
        test_support_core(session)
    }

    #[test]
    fn whole_surface_capture_keeps_original_origin_and_order_not_first_tab_context() {
        let mut session = session_with_surface(json!({
            "name":"Collection", "view_sets":[
                {"schema":2,"id":"one","title":"One","root":{"type":"group","views":["view:test/library"],"active":0}},
                {"schema":2,"id":"two","title":"Two","root":{"type":"group","views":["view:test/detail"],"active":0}}
            ],
            "views":{"view:test/library":{"widget":"rows"},"view:test/detail":{"widget":"rows"}}
        }));
        session.binding_attachments[0].subject =
            crate::ui::binding::UiBindingSubject::RegisteredProject {
                local_id: "collection-project".into(),
            };
        let core = test_support_core(session);
        let surface = core.active_surface().unwrap();
        let origin_instance = surface.view_sets()[0]
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        let expected_ref = core
            .binding_attachment(&surface.binding().binding_attachment_id)
            .unwrap()
            .root_ref
            .clone();
        let (origin, title, active, templates) =
            core.capture_saved_surface(&origin_instance).unwrap();
        assert_eq!(origin.surface_ref, expected_ref);
        assert_eq!(title, "Collection");
        assert_eq!(
            templates
                .iter()
                .map(|(_, tab)| tab.name.as_str())
                .collect::<Vec<_>>(),
            vec!["One", "Two"]
        );
        assert_eq!(active, templates[0].1.id);
        assert!(
            templates[0].1.relationships.is_empty(),
            "only invoking management tile excluded"
        );
        assert_eq!(templates[1].1.relationships.len(), 1);
    }

    #[test]
    fn whole_surface_capture_refuses_unregistered_original_origin() {
        let mut session = session_with_surface(json!({
            "name":"Collection","tiles":["view:test/library"],
            "views":{"view:test/library":{"widget":"rows"}}
        }));
        let id = session.presentations[0].surfaces[0]
            .binding
            .binding_attachment_id
            .clone();
        session
            .binding_attachments
            .iter_mut()
            .find(|binding| binding.binding_attachment_id == id)
            .unwrap()
            .subject = crate::ui::binding::UiBindingSubject::UnregisteredProject;
        let core = test_support_core(session);
        let instance = core
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        assert!(
            core.capture_saved_surface(&instance)
                .unwrap_err()
                .contains("register this project")
        );
    }

    #[test]
    fn whole_surface_save_preserves_observed_revision_and_signed_companion() {
        let mut core = test_support_core(session_with_surface(json!({
            "name":"Collection","tiles":["view:test/library"],
            "views":{"view:test/library":{"widget":"rows","affordances":[{
                "id":"persist","producer":"selection","invoke":{"plane":"rye",
                    "ref":"service:test/persist","args":{"reusable_surface_library":{
                        "expected_revision":"{record.expected_revision}",
                        "reusable_surfaces":"{record.reusable_surfaces}"}}}
            }]}}
        })));
        core.binding_attachments
            .get_mut("fixture-attachment")
            .unwrap()
            .descriptor
            .subject = crate::ui::binding::UiBindingSubject::RegisteredProject {
            local_id: "collection-project".into(),
        };
        let instance = core
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        let effects = core.save_surface_record(
            &instance,
            "view:test/library",
            "persist",
            json!({"expected_revision":9,"reusable_surfaces":[]}),
        );
        assert_eq!(effects.len(), 1);
        let RyeOsEffectKind::InvokeBinding { request, .. } = &effects[0].kind else {
            panic!("signed companion")
        };
        let UiBindingPayload::Selection { record } = &request.payload else {
            panic!("selection")
        };
        assert_eq!(record["expected_revision"], 9);
        assert_eq!(record["reusable_surfaces"].as_array().unwrap().len(), 1);
        assert!(record["reusable_surfaces"][0].get("binding").is_none());
    }

    #[test]
    fn save_uses_companion_coordinate_and_retains_observed_revision() {
        let mut core = core();
        let instance = core
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        let effects = core.save_view_set_record(
            &instance,
            "view:test/library",
            "persist",
            json!({
                "expected_revision": 7, "saved_view_sets": []
            }),
        );
        assert_eq!(effects.len(), 1);
        let RyeOsEffectKind::InvokeBinding { request, .. } = &effects[0].kind else {
            panic!("signed invocation")
        };
        assert_eq!(
            request.coordinate,
            UiBindingCoordinate::Affordance {
                view_ref: "view:test/library".into(),
                affordance_id: "persist".into()
            }
            context
                .saved_view_sets
                .push(self.capture_view_set_template_with_relationships(
                    id,
                    title,
                    Some(instance),
                    &self.view_set_template_ids,
                )?);
            validate_saved_view_set_templates(&context.saved_view_sets)?;
            Ok(
                json!({ "captured": true, "expected_revision": context.expected_revision, "saved_view_sets": context.saved_view_sets }),
            )
            .is_empty()
        );
    }

    #[test]
    fn particular_save_uses_exact_destination_subject_not_library_source_metadata() {
        let mut core = core();
        let instance = core
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        let destination = core.composition_destination(&instance).unwrap();
        let attachment_id = core
            .insertion_attachment_id(destination.view_set_id)
            .unwrap()
            .to_owned();
        core.binding_attachments
            .get_mut(&attachment_id)
            .unwrap()
            .descriptor
            .subject = crate::ui::binding::UiBindingSubject::RegisteredProject {
            local_id: "destination-project".into(),
        };
        let effects = core.save_particular_view_set_record(
            &instance,
            "view:test/library",
            "persist",
            json!({"expected_revision":7,"particular_view_sets":[]}),
        );
        let RyeOsEffectKind::InvokeBinding { request, .. } = &effects[0].kind else {
            panic!("signed invocation")
        };
        let UiBindingPayload::Selection { record } = &request.payload else {
            panic!("selection producer")
        };
        assert_eq!(
            record["particular_view_sets"][0]["context"]["project"]["local_id"],
            "destination-project"
        );
        assert_eq!(
            record["particular_view_sets"][0]["context"]["surface_ref"],
            "surface:ryeos/ryeos/base"
        );
        assert!(
            serde_json::from_value::<ParticularSaveContext>(json!({
                "expected_revision":7,"particular_view_sets":[],"project_local_id":"source-project"
            }))
            .is_err()
        );
    }

    #[test]
    fn particular_save_refuses_unregistered_project_even_with_selected_work() {
        let mut core = core();
        let instance = core
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        let selection_key = crate::ui::seat::selection_storage_key(
            core.active_view_set().unwrap().id,
            "selection.work",
        )
        .unwrap();
        core.seat.append_facet(
            selection_key,
            json!({"thread":"T-current","chain_root":"T-root"}),
        );
        let effects = core.save_particular_view_set_record(
            &instance,
            "view:test/library",
            "persist",
            json!({"expected_revision":7,"particular_view_sets":[]}),
        );
        assert!(effects.is_empty());
        assert!(
            core.ui
                .notices
                .last()
                .unwrap()
                .message
                .contains("register this project")
        );
    }

    #[test]
    fn reusable_pin_opens_unresolved_without_using_ambient_subject() {
        let session = session_with_surface(json!({
            "name": "test",
            "tiles": ["view:test/library"],
            "views": {
                "view:test/library": {"widget": "rows"},
                "view:test/detail": {
                    "widget": "rows",
                    "sources": {"default": {
                        "ref": "service:test/detail",
                        "params": {"thread": "@facet:selection.work.thread"}
                    }}
                }
                self.invoke_affordance(instance, view_ref, persist_id, &record)
            }
        }));
        let mut core = test_support_core(session);
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let library = core
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        let selection_key = crate::ui::seat::selection_storage_key(
            core.active_view_set().unwrap().id,
            "selection.work",
        )
        .unwrap();
        core.seat.append_facet(
            selection_key,
            json!({"thread": "T-current", "chain_root": "T-root"}),
        );
        let effects = core.open_view_set_record(
            &library,
            json!({
                "id": "detail",
                "name": "Detail",
                "composition": {
                    "schema": 2,
                    "id": "detail",
                    "title": "Detail",
                    "root": {"type":"group", "views":["view:test/detail"], "active":0}
                },
                "relationships": [{
                    "mount": {"kind":"tile", "index":0},
                    "source": {
                        "mode":"required_subject",
                        "input":"subject_tile_0",
                        "facets":["selection.work.thread"]
                    }
                }]
            }),
        );
        assert!(effects.is_empty());
        assert_eq!(core.active_surface().unwrap().view_sets().len(), 1);
        settle_pending_prefix(&mut core, &producer);
        assert_eq!(core.active_surface().unwrap().view_sets().len(), 2);
        let opened = core
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        assert_eq!(
            core.selection_attachments.get(&opened),
            Some(
                &crate::ui::attachment::SelectionAttachment::RequiredSubject {
                    input: "subject_tile_0".into(),
                    facets: vec!["selection.work.thread".into()],
                }
                self.invoke_affordance(instance, view_ref, persist_id, &record)
            }
            Err(error) => {
                self.notice(
                    format!("Cannot retain particular view set: {error}"),
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
    use crate::ui::binding::{UiBindingCoordinate, UiBindingPayload};
    use crate::ui::effect::RyeOsEffectKind;
    use crate::ui::model::{BrowserViewport, RyeOsCore};
    use crate::ui::reducer::test_support::session_with_surface;

    fn core() -> RyeOsCore {
        let session = session_with_surface(json!({
            "name": "test",
            "tiles": ["view:test/library"],
            "views": {
                "view:test/library": {
                    "widget": "rows",
                    "affordances": [{
                        "id": "persist", "producer": "selection",
                        "invoke": { "plane": "rye", "ref": "service:test/persist", "args": {
                            "view_set_library": {
                                "expected_revision": "{record.expected_revision}",
                                "saved_view_sets": "{record.saved_view_sets}"
                            }
                        } }
                    }]
                }
            }
        }));
        RyeOsCore::new(session, BrowserViewport::default(), 0)
    }

    #[test]
    fn save_uses_companion_coordinate_and_retains_observed_revision() {
        let mut core = core();
        let instance = core
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        let effects = core.save_view_set_record(
            &instance,
            "view:test/library",
            "persist",
            json!({
                "expected_revision": 7, "saved_view_sets": []
            }),
        );
        assert_eq!(effects.len(), 1);
        let RyeOsEffectKind::InvokeBinding { request, .. } = &effects[0].kind else {
            panic!("signed invocation")
        };
        assert_eq!(
            request.coordinate,
            UiBindingCoordinate::Affordance {
                view_ref: "view:test/library".into(),
                affordance_id: "persist".into()
            }
        );
        let UiBindingPayload::Selection { record } = &request.payload else {
            panic!("selection producer")
        };
        assert_eq!(record["expected_revision"], 7);
        let templates: Vec<SavedViewSetTemplate> =
            serde_json::from_value(record["saved_view_sets"].clone()).unwrap();
        assert_eq!(templates.len(), 1);
        validate_saved_view_set_templates(&templates).unwrap();
        // A second save is neither an automatic overwrite nor an implicit retry.
        assert!(
            core.save_view_set_record(
                &instance,
                "view:test/library",
                "persist",
                json!({
                    "expected_revision": 8, "saved_view_sets": templates
                })
            )
            .is_empty()
        );
    }

    #[test]
    fn save_does_not_dispatch_without_a_current_compiled_binding() {
        let mut core = core();
        let instance = core
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        let selection_key =
            crate::ui::seat::selection_storage_key(core.view_sets[0].id, "selection.work").unwrap();
        core.seat.append_facet(
            selection_key,
            json!({"thread": "T-current", "chain_root": "T-root"}),
        );
        let effects = core.open_view_set_record(
            &library,
            json!({
                "id": "detail",
                "name": "Detail",
                "composition": {
                    "id": "detail",
                    "title": "Detail",
                    "root": {"type":"group", "views":["view:test/detail"], "active":0},
                    "slots": {}
                },
                "relationships": [{
                    "mount": {"kind":"tile", "index":0},
                    "source": {
                        "mode":"required_subject",
                        "input":"subject_tile_0",
                        "facets":["selection.work.thread"]
                    }
                }]
            }),
        );
        assert!(effects.is_empty());
        let opened = core.view_sets[core.active_view_set]
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        assert_eq!(
            core.selection_attachments.get(&opened),
            Some(
                &crate::ui::attachment::SelectionAttachment::RequiredSubject {
                    input: "subject_tile_0".into(),
                    facets: vec!["selection.work.thread".into()],
                }
            )
        );
    }

    #[test]
    fn save_cannot_recurse_into_a_local_companion() {
        let mut core = core();
        let instance = core.view_sets[0]
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        core.binding_attachments
            .get_mut("fixture-attachment")
            .unwrap()
            .views
            .get_mut("view:test/library")
            .unwrap()
            .affordances[0]["invoke"]["plane"] = json!("ui");
        assert!(
            core.save_view_set_record(
                &instance,
                "view:test/library",
                "persist",
                json!({
                    "expected_revision": 7, "saved_view_sets": []
                })
            )
            .is_empty()
        );
        assert!(core.pending_effects.is_empty());
    }

    #[test]
    fn save_does_not_dispatch_without_a_current_compiled_binding() {
        let mut core = core();
        let instance = core.view_sets[0]
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        core.binding_attachments
            .get_mut("fixture-attachment")
            .unwrap()
            .descriptor
            .binding_digest
            .clear();
        assert!(
            core.save_view_set_record(
                &instance,
                "view:test/library",
                "persist",
                json!({"expected_revision": 0, "saved_view_sets": []})
            )
            .is_empty()
        );
        assert!(core.pending_effects.is_empty());
    }

    #[test]
    fn malformed_library_context_never_dispatches_persistence() {
        let mut core = core();
        let instance = core
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        for value in [
            json!({"saved_view_sets": []}),
            json!({"expected_revision": -1, "saved_view_sets": []}),
            json!({"expected_revision": 0, "saved_view_sets": [], "authority": "operator"}),
        ] {
            assert!(
                core.save_view_set_record(&instance, "view:test/library", "persist", value)
                    .is_empty()
            );
        }
        assert!(core.pending_effects.is_empty());
    }

    #[test]
    fn save_checks_compiled_envelope_limit_before_emitting_effect() {
        let mut core = core();
        let instance = core
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        core.binding_attachments
            .get_mut("fixture-attachment")
            .unwrap()
            .descriptor
            .binding_request_bounds
            .max_request_bytes = 1;
        assert!(
            core.save_view_set_record(
                &instance,
                "view:test/library",
                "persist",
                json!({"expected_revision": 0, "saved_view_sets": []})
            )
            .is_empty()
        );
        assert!(core.pending_effects.is_empty());
    }

    #[test]
    fn save_transport_budget_counts_the_complete_binding_envelope() {
        let mut first = core();
        let instance = first
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        let effects = first.save_view_set_record(
            &instance,
            "view:test/library",
            "persist",
            json!({"expected_revision": 0, "saved_view_sets": []}),
        );
        let RyeOsEffectKind::InvokeBinding { request, .. } = &effects[0].kind else {
            panic!("expected persistence request")
        };
        let exact_bytes = serde_json::to_vec(request).unwrap().len() as u64;
        for (limit, expected_count) in [(exact_bytes, 1), (exact_bytes - 1, 0)] {
            let mut candidate = core();
            let instance = candidate
                .active_view_set()
                .unwrap()
                .tiles
                .values()
                .next()
                .unwrap()
                .instance_key
                .clone();
            candidate
                .binding_attachments
                .get_mut("fixture-attachment")
                .unwrap()
                .descriptor
                .binding_request_bounds
                .max_request_bytes = limit;
            let effects = candidate.save_view_set_record(
                &instance,
                "view:test/library",
                "persist",
                json!({"expected_revision": 0, "saved_view_sets": []}),
            );
            assert_eq!(effects.len(), expected_count);
        }
    }
}

impl RyeOsCore {
    pub(crate) fn saved_surface_project(
        subject: &crate::ui::binding::UiBindingSubject,
    ) -> Result<Option<ParticularProjectRef>, String> {
        match subject {
            crate::ui::binding::UiBindingSubject::Projectless => Ok(None),
            crate::ui::binding::UiBindingSubject::RegisteredProject { local_id } => {
                Ok(Some(ParticularProjectRef { local_id: local_id.clone() }))
            }
            crate::ui::binding::UiBindingSubject::UnregisteredProject => Err(
                "register this project before saving its Surface; project context cannot be discarded".into(),
            ),
        }
    }

    fn capture_saved_surface(
        &self,
        instance: &RyeOsViewInstanceKey,
    ) -> Result<
        (
            crate::surface::saved_surfaces::SavedSurfaceOrigin,
            String,
            String,
            Vec<(crate::ids::ViewSetId, SavedViewSetTemplate)>,
        ),
        String,
    > {
        if !self.composition_write_available() {
            return Err("settle pending presentation changes before saving a Surface".into());
        }
        let presentation = self
            .presentation
            .as_ref()
            .ok_or("presentation is unavailable")?;
        if presentation
            .mount(instance)
            .is_none_or(|mount| !mount.visible())
        {
            return Err("saving View is no longer a visible exact mount".into());
        }
        let destination = self.composition_destination(instance)?;
        let surface = presentation
            .surfaces()
            .get(&destination.surface_instance_id)
            .ok_or("captured Surface is no longer open")?;
        let replay = self.acknowledged_composition_replay()?;
        let original = replay
            .admitted_surfaces
            .get(destination.surface_instance_id.as_str())
            .filter(|member| &member.binding == surface.binding())
            .ok_or("original admitted Surface origin is unavailable")?;
        let descriptor = self
            .binding_attachment(&original.binding.binding_attachment_id)
            .filter(|descriptor| {
                descriptor.binding_generation == original.binding.binding_generation
                    && descriptor.binding_digest == original.binding.binding_digest
            })
            .ok_or("original admitted Surface descriptor is unavailable")?;
        if descriptor.presentation.definition.surface().is_none() {
            return Err("original composition origin is not a Surface".into());
        }
        let origin = crate::surface::saved_surfaces::SavedSurfaceOrigin {
            surface_ref: descriptor.root_ref.clone(),
            project: Self::saved_surface_project(&descriptor.subject)?,
        };
        let title = surface.title().to_owned();
        let surface_id = format!("{:x}", Sha256::digest(title.as_bytes()));
        let saved_ids = surface
            .view_sets()
            .iter()
            .enumerate()
            .map(|(index, set)| {
                (
                    set.id,
                    format!(
                        "{:x}",
                        Sha256::digest(format!("{surface_id}:{index}:{}", set.title).as_bytes())
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let active = surface
            .active_view_set()
            .and_then(|set| saved_ids.get(&set.id))
            .cloned()
            .ok_or("captured Surface has no active tab to save")?;
        let mut templates = Vec::new();
        for set in surface.view_sets() {
            let exclude = self
                .view_set_for_instance(instance)
                .is_some_and(|owner| owner.id == set.id)
                .then_some(instance);
            let template = self.capture_view_set_template_at(
                set.id,
                saved_ids[&set.id].clone(),
                set.title.clone(),
                exclude,
                &saved_ids,
            )?;
            templates.push((set.id, template));
        }
        Ok((origin, title, active, templates))
    }

    fn persist_saved_surface(
        &mut self,
        instance: &RyeOsViewInstanceKey,
        view_ref: &str,
        persist_id: &str,
        prepared: Result<Value, String>,
    ) -> Vec<RyeOsEffect> {
        let companion = self
            .binding_for_instance(instance, view_ref)
            .and_then(|binding| {
                binding.affordances.iter().find(|affordance| {
                    affordance.get("id").and_then(Value::as_str) == Some(persist_id)
                })
            });
        if !companion.is_some_and(|affordance| {
            affordance.pointer("/invoke/plane").and_then(Value::as_str) == Some("rye")
                && affordance
                    .pointer("/invoke/ref")
                    .and_then(Value::as_str)
                    .is_some()
                && affordance
                    .get("producer")
                    .and_then(Value::as_str)
                    .unwrap_or("selection")
                    == "selection"
        }) {
            self.notice(
                "Surface persistence is not admitted by this View.",
                RyeOsTone::Warn,
            );
            return Vec::new();
        }
        let attempt = prepared.and_then(|record| {
            let (request, bounds) = self
                .compiled_binding_operation(
                    instance,
                    crate::ui::binding::UiBindingCoordinate::Affordance {
                        view_ref: view_ref.into(),
                        affordance_id: persist_id.into(),
                    },
                    crate::ui::binding::UiBindingPayload::Selection {
                        record: record.clone(),
                    },
                )
                .ok_or("Surface persistence binding is unavailable")?;
            request.validate_bounds(bounds).map_err(|error| {
                format!("Surface persistence exceeds admitted request bounds: {error:?}")
            })?;
            Ok(record)
        });
        match attempt {
            Ok(record) => self.invoke_affordance(instance, view_ref, persist_id, &record),
            Err(error) => {
                self.notice(format!("Cannot save Surface: {error}"), RyeOsTone::Warn);
                Vec::new()
            }
        }
    }

    pub(crate) fn save_surface_record(
        &mut self,
        instance: &RyeOsViewInstanceKey,
        view_ref: &str,
        persist_id: &str,
        value: Value,
    ) -> Vec<RyeOsEffect> {
        use crate::surface::saved_surfaces::{
            ReusableSurfaceRecord, validate_saved_surface_libraries,
        };
        let prepared = (|| {
            let mut context: SurfaceSaveContext =
                serde_json::from_value(value).map_err(|error| error.to_string())?;
            validate_saved_surface_libraries(&context.reusable_surfaces, &[], &[])?;
            let (origin, title, active, templates) = self.capture_saved_surface(instance)?;
            let id = format!("{:x}", Sha256::digest(title.as_bytes()));
            if context
                .reusable_surfaces
                .iter()
                .any(|record| record.id == id || record.title == title)
            {
                return Err(
                    "that Surface name is already saved; rename it before saving another".into(),
                );
            }
            let tab_origins = templates
                .iter()
                .map(|(set_id, template)| {
                    let descriptor = self
                        .insertion_attachment_id(*set_id)
                        .and_then(|id| self.binding_attachment(id))
                        .ok_or("captured tab insertion context is unavailable")?;
                    if descriptor.presentation.definition.surface().is_none() {
                        return Err("captured tab origin is not an admitted Surface".to_owned());
                    }
                    Ok((
                        template.id.clone(),
                        crate::surface::saved_surfaces::SavedSurfaceOrigin {
                            surface_ref: descriptor.root_ref.clone(),
                            project: Self::saved_surface_project(&descriptor.subject)?,
                        },
                    ))
                })
                .collect::<Result<BTreeMap<_, _>, String>>()?;
            context.reusable_surfaces.push(ReusableSurfaceRecord {
                id,
                title,
                origin,
                active_view_set_id: active,
                view_sets: templates
                    .into_iter()
                    .map(|(_, template)| template)
                    .collect(),
                tab_origins,
            });
            validate_saved_surface_libraries(&context.reusable_surfaces, &[], &[])?;
            Ok(
                json!({"captured":true,"expected_revision":context.expected_revision,"reusable_surfaces":context.reusable_surfaces}),
            )
        })();
        self.persist_saved_surface(instance, view_ref, persist_id, prepared)
    }

    pub(crate) fn save_particular_surface_record(
        &mut self,
        instance: &RyeOsViewInstanceKey,
        view_ref: &str,
        persist_id: &str,
        value: Value,
    ) -> Vec<RyeOsEffect> {
        use crate::surface::saved_surfaces::{
            ParticularSurfaceRecord, validate_saved_surface_libraries,
        };
        let prepared = (|| {
            let mut context: ParticularSurfaceSaveContext =
                serde_json::from_value(value).map_err(|error| error.to_string())?;
            validate_particular_view_set_resumes(&context.particular_view_sets)?;
            validate_saved_surface_libraries(
                &[],
                &context.particular_surfaces,
                &context.particular_view_sets,
            )?;
            let (origin, title, active, templates) = self.capture_saved_surface(instance)?;
            let id = format!("{:x}", Sha256::digest(title.as_bytes()));
            if context
                .particular_surfaces
                .iter()
                .any(|record| record.id == id || record.title == title)
            {
                return Err(
                    "that Surface name is already retained; rename it before retaining another"
                        .into(),
                );
            }
            let mut view_set_ids = Vec::new();
            for (set_id, template) in templates {
                if context
                    .particular_view_sets
                    .iter()
                    .any(|record| record.id == template.id)
                {
                    return Err(
                        "captured Surface tab identity already exists in the particular library"
                            .into(),
                    );
                }
                let descriptor = self
                    .insertion_attachment_id(set_id)
                    .and_then(|id| self.binding_attachment(id))
                    .ok_or("captured tab insertion context is unavailable")?;
                let project = Self::saved_surface_project(&descriptor.subject)?;
                let work = self
                    .selection_value_in_view_set(set_id, "selection.work")
                    .and_then(|value| {
                        value
                            .get("chain_root")
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty())
                            .map(|value| ParticularWorkRef {
                                chain_root_id: value.to_owned(),
                            })
                    });
                let record = ParticularViewSetResume {
                    id: template.id.clone(),
                    name: template.name,
                    composition: template.composition,
                    relationships: template.relationships,
                    context: ParticularViewSetContext {
                        surface_ref: descriptor.root_ref.clone(),
                        project,
                        work,
                    },
                };
                view_set_ids.push(record.id.clone());
                context.particular_view_sets.push(record);
            }
            validate_particular_view_set_resumes(&context.particular_view_sets)?;
            context.particular_surfaces.push(ParticularSurfaceRecord {
                id,
                title,
                origin,
                view_set_ids,
                active_view_set_id: active,
            });
            validate_saved_surface_libraries(
                &[],
                &context.particular_surfaces,
                &context.particular_view_sets,
            )?;
            Ok(
                json!({"captured":true,"expected_revision":context.expected_revision,
                "particular_surfaces":context.particular_surfaces,"view_set_expected_revision":context.view_set_expected_revision,
                "particular_view_sets":context.particular_view_sets}),
            )
        })();
        self.persist_saved_surface(instance, view_ref, persist_id, prepared)
    }

    /// Management actions keep the originating set/destination even after
    /// focus changes. Shared docks have no inferred current-tab destination.
    pub(crate) fn composition_destination(
        &self,
        instance: &RyeOsViewInstanceKey,
    ) -> Result<crate::ui::presentation_state::PresentationInsertionDestination, String> {
        let composition = self
            .presentation
            .as_ref()
            .ok_or("presentation is unavailable")?;
        if let Some((surface, set)) = composition.view_set_for_instance(instance) {
            return Ok(
                crate::ui::presentation_state::PresentationInsertionDestination {
                    surface_instance_id: surface.clone(),
                    view_set_id: set.id,
                },
            );
        }
        let destination = composition
            .dialogs()
            .values()
            .find(|dialog| dialog.view().instance_key() == instance)
            .and_then(|dialog| dialog.insertion())
            .cloned()
            .ok_or("this shared view has no retained composition destination")?;
        composition
            .view_set(&destination.surface_instance_id, destination.view_set_id)
            .ok_or("captured composition destination is no longer mounted")?;
        Ok(destination)
    }
    pub(crate) fn open_view_set_record(
        &mut self,
        instance: &RyeOsViewInstanceKey,
        value: Value,
    ) -> Vec<RyeOsEffect> {
        let result = (|| -> Result<Vec<RyeOsEffect>, String> {
            // Shared library/dialog authority is not its insertion authority.
            // Keep the retained invoking destination even after tab/Surface
            // focus changes, and use that tab's exact admitted context.
            if self
                .presentation
                .as_ref()
                .and_then(|presentation| presentation.mount(instance))
                .is_none_or(|mount| !mount.visible())
            {
                return Err("library View is no longer a visible exact mount".into());
            }
            self.binding_attachment_for_instance(instance)
                .ok_or("library View's admitted binding is unavailable")?;
            let destination = self.composition_destination(instance)?;
            let attachment_id = self
                .insertion_attachment_id(destination.view_set_id)
                .ok_or("captured tab has no admitted insertion context")?
                .to_owned();
            let template = serde_json::from_value::<SavedViewSetTemplate>(value)
                .map_err(|error| error.to_string())?;
            validate_saved_view_set_templates(std::slice::from_ref(&template))?;
            let required_sets = template
                .relationships
                .iter()
                .filter_map(|relationship| {
                    if let SavedViewSelectionSource::FollowSet { saved_view_set_id } =
                        &relationship.source
                    {
                        Some(saved_view_set_id.as_str())
                    } else {
                        None
                    }
                })
                .collect::<std::collections::BTreeSet<_>>();
            let saved_sets = self
                .live_saved_set_resolutions_at(&destination.surface_instance_id, &required_sets)?;
            self.open_saved_view_set_template_with_policy_at(
                &template,
                &attachment_id,
                &BTreeMap::new(),
                &saved_sets,
                crate::ui::layout_preferences::RequiredSubjectMountPolicy::AllowUnresolved,
                &destination,
            )
        })();
        match result {
            Ok(effects) => effects,
            Err(error) => {
                self.notice(
                    format!("Cannot open saved view set: {error}"),
                    RyeOsTone::Warn,
                );
                Vec::new()
            }
        }
    }

    pub(crate) fn save_view_set_record(
        &mut self,
        instance: &RyeOsViewInstanceKey,
        view_ref: &str,
        persist_id: &str,
        value: Value,
    ) -> Vec<RyeOsEffect> {
        // The companion must be executable, never another local operation:
        // this prevents cycles and leaves its target solely in signed content.
        let companion = self
            .binding_for_instance(instance, view_ref)
            .and_then(|binding| {
                binding
                    .affordances
                    .iter()
                    .find(|a| a.get("id").and_then(Value::as_str) == Some(persist_id))
            });
        if !companion.is_some_and(|a| {
            a.pointer("/invoke/plane").and_then(Value::as_str) == Some("rye")
                && a.pointer("/invoke/ref").and_then(Value::as_str).is_some()
                && a.get("producer")
                    .and_then(Value::as_str)
                    .unwrap_or("selection")
                    == "selection"
        }) {
            self.notice(
                "Saved-set persistence is not admitted by this view.",
                RyeOsTone::Warn,
            );
            return Vec::new();
        }
        let prepared = (|| -> Result<Value, String> {
            let mut context: SaveContext =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            validate_saved_view_set_templates(&context.saved_view_sets)?;
            let destination = self.composition_destination(instance)?;
            let title = self
                .view_set(destination.view_set_id)
                .ok_or("captured view set is unavailable")?
                .title
                .clone();
            let id = format!("{:x}", Sha256::digest(title.as_bytes()));
            if context
                .saved_view_sets
                .iter()
                .any(|template| template.id == id || template.name == title)
            {
                return Err("that name is already saved; rename the open set before saving a new composition".into());
            }
            context
                .saved_view_sets
                .push(self.capture_view_set_template_with_relationships(
                    id,
                    title,
                    Some(instance),
                    &self.view_set_template_ids,
                )?);
            validate_saved_view_set_templates(&context.saved_view_sets)?;
            Ok(
                json!({ "captured": true, "expected_revision": context.expected_revision, "saved_view_sets": context.saved_view_sets }),
            )
        })();
        match prepared {
            Ok(record) => {
                // The durable library ceiling is not the session's transport
                // allowance. Check the complete envelope against compiled
                // bounds, rather than hardcoding a guessed payload reserve.
                let Some((request, bounds)) = self.compiled_binding_operation(
                    instance,
                    crate::ui::binding::UiBindingCoordinate::Affordance {
                        view_ref: view_ref.into(),
                        affordance_id: persist_id.into(),
                    },
                    crate::ui::binding::UiBindingPayload::Selection {
                        record: record.clone(),
                    },
                ) else {
                    self.notice(
                        "Cannot save view set: its admitted binding is unavailable.",
                        RyeOsTone::Warn,
                    );
                    return Vec::new();
                };
                if let Err(error) = request.validate_bounds(bounds) {
                    self.notice(
                        format!("Cannot save view set through this session: {error:?}"),
                        RyeOsTone::Warn,
                    );
                    return Vec::new();
                }
                self.invoke_affordance(instance, view_ref, persist_id, &record)
            }
            Err(error) => {
                self.notice(format!("Cannot save view set: {error}"), RyeOsTone::Warn);
                Vec::new()
            }
        }
    }

    pub(crate) fn save_particular_view_set_record(
        &mut self,
        instance: &RyeOsViewInstanceKey,
        view_ref: &str,
        persist_id: &str,
        value: Value,
    ) -> Vec<RyeOsEffect> {
        let companion = self
            .binding_for_instance(instance, view_ref)
            .and_then(|binding| {
                binding.affordances.iter().find(|affordance| {
                    affordance.get("id").and_then(Value::as_str) == Some(persist_id)
                })
            });
        if !companion.is_some_and(|affordance| {
            affordance.pointer("/invoke/plane").and_then(Value::as_str) == Some("rye")
                && affordance
                    .pointer("/invoke/ref")
                    .and_then(Value::as_str)
                    .is_some()
                && affordance
                    .get("producer")
                    .and_then(Value::as_str)
                    .unwrap_or("selection")
                    == "selection"
        }) {
            self.notice(
                "Particular-set persistence is not admitted by this view.",
                RyeOsTone::Warn,
            );
            return Vec::new();
        }

        let prepared = (|| -> Result<Value, String> {
            let mut context: ParticularSaveContext =
                serde_json::from_value(value).map_err(|error| error.to_string())?;
            validate_particular_view_set_resumes(&context.particular_view_sets)?;
            let destination = self.composition_destination(instance)?;
            let destination_attachment = self
                .insertion_attachment_id(destination.view_set_id)
                .and_then(|attachment_id| self.binding_attachment(attachment_id))
                .ok_or("captured Surface definition is unavailable")?;
            let surface_ref = destination_attachment.root_ref.clone();
            let project = match &destination_attachment.subject {
                crate::ui::binding::UiBindingSubject::RegisteredProject { local_id } => {
                    Some(ParticularProjectRef {
                        local_id: local_id.clone(),
                    })
                }
                crate::ui::binding::UiBindingSubject::Projectless => None,
                crate::ui::binding::UiBindingSubject::UnregisteredProject => {
                    return Err("register this project before retaining a particular view set; its project context cannot be discarded".into());
                }
            };
            let (view_set_id, title) = self
                .view_set(destination.view_set_id)
                .map(|view_set| (view_set.id, view_set.title.clone()))
                .ok_or("active view set is unavailable")?;
            let id = format!("{:x}", Sha256::digest(title.as_bytes()));
            if context
                .particular_view_sets
                .iter()
                .any(|candidate| candidate.id == id || candidate.name == title)
            {
                return Err(
                    "that name is already retained; rename the open set before retaining another"
                        .into(),
                );
            }
            let template = self.capture_view_set_template_with_relationships(
                id.clone(),
                title.clone(),
                Some(instance),
                &self.view_set_template_ids,
            )?;
            let work = self
                .selection_value_in_view_set(view_set_id, "selection.work")
                .and_then(|selection| {
                    selection
                        .get("chain_root")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .map(|chain_root_id| ParticularWorkRef {
                            chain_root_id: chain_root_id.to_owned(),
                        })
                });
            if project.is_none() && work.is_none() {
                return Err(
                    "a particular view set requires a registered project or selected logical work"
                        .into(),
                );
            }
            context.particular_view_sets.push(ParticularViewSetResume {
                id,
                name: title,
                composition: template.composition,
                relationships: template.relationships,
                context: ParticularViewSetContext {
                    surface_ref,
                    project,
                    work,
                },
            });
            validate_particular_view_set_resumes(&context.particular_view_sets)?;
            Ok(json!({
                "captured": true,
                "expected_revision": context.expected_revision,
                "particular_view_sets": context.particular_view_sets,
            }))
        })();

        match prepared {
            Ok(record) => {
                let Some((request, bounds)) = self.compiled_binding_operation(
                    instance,
                    crate::ui::binding::UiBindingCoordinate::Affordance {
                        view_ref: view_ref.into(),
                        affordance_id: persist_id.into(),
                    },
                    crate::ui::binding::UiBindingPayload::Selection {
                        record: record.clone(),
                    },
                ) else {
                    self.notice(
                        "Cannot retain particular view set: its admitted binding is unavailable.",
                        RyeOsTone::Warn,
                    );
                    return Vec::new();
                };
                if let Err(error) = request.validate_bounds(bounds) {
                    self.notice(
                        format!(
                            "Cannot retain particular view set through this session: {error:?}"
                        ),
                        RyeOsTone::Warn,
                    );
                    return Vec::new();
                }
                self.invoke_affordance(instance, view_ref, persist_id, &record)
            }
            Err(error) => {
                self.notice(
                    format!("Cannot retain particular view set: {error}"),
                    RyeOsTone::Warn,
                );
                Vec::new()
            }
        }
    }
}
