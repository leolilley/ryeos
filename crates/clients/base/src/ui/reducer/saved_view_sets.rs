//! Local composition operations. Persistence always uses the signed companion
//! affordance; a client-produced template never supplies execution authority.

use super::effect::RyeOsEffect;
use super::model::RyeOsCore;
use super::view_model::RyeOsTone;
use crate::ids::RyeOsViewInstanceKey;
use crate::surface::view_sets::{SavedViewSetTemplate, validate_saved_view_set_templates};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveContext {
    expected_revision: u64,
    saved_view_sets: Vec<SavedViewSetTemplate>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::binding::{UiBindingCoordinate, UiBindingPayload};
    use crate::ui::effect::RyeOsEffectKind;
    use crate::ui::model::{BrowserViewport, RyeOsCore};
    use crate::ui::reducer::test_support::writable_session;

    fn core() -> RyeOsCore {
        let mut session = writable_session();
        session.effective_surface = Some(json!({
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
    fn save_cannot_recurse_into_a_local_companion() {
        let mut core = core();
        let instance = core.view_sets[0]
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        core.views.get_mut("view:test/library").unwrap().affordances[0]["invoke"]["plane"] =
            json!("ui");
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
        core.data.session.as_mut().unwrap().binding_digest.clear();
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
}

impl RyeOsCore {
    pub(crate) fn open_view_set_record(&mut self, value: Value) -> Vec<RyeOsEffect> {
        let result = serde_json::from_value::<SavedViewSetTemplate>(value)
            .map_err(|error| error.to_string())
            .and_then(|template| self.open_saved_view_set_template(&template));
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
        let companion = self.views.get(view_ref).and_then(|binding| {
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
                .push(self.export_active_view_set_template(id, title)?);
            validate_saved_view_set_templates(&context.saved_view_sets)?;
            Ok(
                json!({ "captured": true, "expected_revision": context.expected_revision, "saved_view_sets": context.saved_view_sets }),
            )
        })();
        match prepared {
            Ok(record) => self.invoke_affordance(instance, view_ref, persist_id, &record),
            Err(error) => {
                self.notice(format!("Cannot save view set: {error}"), RyeOsTone::Warn);
                Vec::new()
            }
        }
    }
}
