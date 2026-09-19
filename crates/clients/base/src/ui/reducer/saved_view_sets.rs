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
    #[serde(default)]
    project_local_id: Option<String>,
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
        let instance = core.view_sets[0]
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
    fn reusable_pin_is_reopened_from_the_current_explicit_subject() {
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
            }
        }));
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        let library = core.view_sets[0]
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
        assert_eq!(effects.len(), 1);
        let opened = core.view_sets[core.active_view_set]
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        let Some(crate::ui::attachment::SelectionAttachment::Pinned { values, .. }) =
            core.selection_attachments.get(&opened)
        else {
            panic!("reusable pin must be supplied afresh")
        };
        assert_eq!(values["selection.work.thread"], "T-current");
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
        let instance = core.view_sets[0]
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
        let instance = first.view_sets[0]
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
            let instance = candidate.view_sets[0]
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
    pub(crate) fn open_view_set_record(
        &mut self,
        instance: &RyeOsViewInstanceKey,
        value: Value,
    ) -> Vec<RyeOsEffect> {
        let Some(attachment_id) = self
            .binding_attachment_for_instance(instance)
            .map(|attachment| attachment.binding_attachment_id.clone())
        else {
            self.notice(
                "Cannot open saved view set: its admitted binding is unavailable.",
                RyeOsTone::Warn,
            );
            return Vec::new();
        };
        let result = (|| -> Result<Vec<RyeOsEffect>, String> {
            let template = serde_json::from_value::<SavedViewSetTemplate>(value)
                .map_err(|error| error.to_string())?;
            validate_saved_view_set_templates(std::slice::from_ref(&template))?;
            let mut fresh_subjects = BTreeMap::new();
            for relationship in &template.relationships {
                let SavedViewSelectionSource::RequiredSubject { input, facets } =
                    &relationship.source
                else {
                    continue;
                };
                let values = fresh_subjects
                    .entry(input.clone())
                    .or_insert_with(BTreeMap::new);
                for facet in facets {
                    let reference = Value::String(format!("@facet:{facet}"));
                    let resolved = super::content::resolve_params(&reference, |key| {
                        self.facet_value_for_instance(instance, key)
                    });
                    if !resolved.is_null() {
                        values.insert(facet.clone(), resolved);
                    }
                }
            }
            let saved_sets = if template.relationships.iter().any(|relationship| {
                matches!(
                    &relationship.source,
                    SavedViewSelectionSource::FollowSet { .. }
                )
            }) {
                self.live_saved_set_resolutions()?
            } else {
                BTreeMap::new()
            };
            self.open_saved_view_set_template_with_relationships(
                &template,
                &attachment_id,
                &fresh_subjects,
                &saved_sets,
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
            let title = self.view_sets[self.active_view_set].title.clone();
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
            let (view_set_id, title) = self
                .view_sets
                .get(self.active_view_set)
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
            let work = crate::ui::seat::selection_storage_key(view_set_id, "selection.work")
                .and_then(|key| self.seat.fold().get(&key).cloned())
                .and_then(|selection| {
                    selection
                        .get("chain_root")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .map(|chain_root_id| ParticularWorkRef {
                            chain_root_id: chain_root_id.to_owned(),
                        })
                });
            let project = context
                .project_local_id
                .take()
                .filter(|value| !value.is_empty())
                .map(|local_id| ParticularProjectRef { local_id });
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
                context: ParticularViewSetContext { project, work },
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
