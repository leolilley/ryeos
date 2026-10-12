//! Exact retained binding ownership for mounted view instances.

use std::collections::BTreeMap;

use super::binding::UiBindingAttachment;
use super::content::{SourceBinding, ViewBinding};
use super::model::RyeOsCore;
use crate::ids::{RyeOsViewInstanceKey, ViewSetId};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetainedUiBindingAttachment {
    pub descriptor: UiBindingAttachment,
    pub surface_sources: BTreeMap<String, SourceBinding>,
    pub views: BTreeMap<String, ViewBinding>,
    #[serde(default)]
    pub initial_input_route: super::seat::InputRoute,
}

use serde::{Deserialize, Serialize};

impl RetainedUiBindingAttachment {
    pub(crate) fn from_descriptor(
        descriptor: UiBindingAttachment,
    ) -> Result<Self, serde_json::Error> {
        descriptor
            .validate()
            .map_err(<serde_json::Error as serde::de::Error>::custom)?;
        let (surface_sources, initial_input_route) = match &descriptor.presentation.definition {
            crate::presentation::UiCompositionDefinition::Surface { surface } => {
                let declaration = serde_json::to_value(surface)?;
                crate::surface::view_sets::validate_effective_view_sets(&declaration)
                    .map_err(<serde_json::Error as serde::de::Error>::custom)?;
                (
                    surface.sources.clone(),
                    super::seat::InputRoute::from_surface_input(surface.input.as_ref())
                        .unwrap_or_default(),
                )
            }
            crate::presentation::UiCompositionDefinition::SharedPresentation { .. } => {
                (BTreeMap::new(), super::seat::InputRoute::default())
            }
        };
        // Adapt only the separate View map to the existing content parser's
        // input shape. This carries no Config-as-Surface identity or authority.
        let view_content = serde_json::json!({"views": descriptor.presentation.views});
        Ok(Self {
            initial_input_route,
            surface_sources,
            views: super::content::views_from_surface(Some(&view_content)),
            descriptor,
        })
    }
}

impl RyeOsCore {
    pub fn active_surface_attachment(&self) -> Option<&UiBindingAttachment> {
        let coordinate = self.active_surface()?.binding();
        let descriptor = self.binding_attachment(&coordinate.binding_attachment_id)?;
        (descriptor.binding_generation == coordinate.binding_generation
            && descriptor.binding_digest == coordinate.binding_digest)
            .then_some(descriptor)
    }
    pub(crate) fn binding_attachment(&self, attachment_id: &str) -> Option<&UiBindingAttachment> {
        self.binding_attachments
            .get(attachment_id)
            .map(|attachment| &attachment.descriptor)
    }

    pub(crate) fn binding_attachment_for_instance(
        &self,
        instance: &RyeOsViewInstanceKey,
    ) -> Option<&UiBindingAttachment> {
        self.binding_context_for_instance(instance)
            .map(|attachment| &attachment.descriptor)
    }

    pub(crate) fn binding_context_for_instance(
        &self,
        instance: &RyeOsViewInstanceKey,
    ) -> Option<&RetainedUiBindingAttachment> {
        let mount = self.presentation.as_ref()?.mount(instance)?;
        let coordinate = mount.binding();
        let retained = self
            .binding_attachments
            .get(&coordinate.binding_attachment_id)?;
        (retained.descriptor.binding_generation == coordinate.binding_generation
            && retained.descriptor.binding_digest == coordinate.binding_digest)
            .then_some(retained)
    }

    pub(crate) fn binding_for_instance(
        &self,
        instance: &RyeOsViewInstanceKey,
        view_ref: &str,
    ) -> Option<&ViewBinding> {
        if self.mounted_view_ref(instance) != Some(view_ref) {
            return None;
        }
        self.binding_context_for_instance(instance)?
            .views
            .get(view_ref)
    }

    pub(crate) fn binding_for_insertion(
        &self,
        view_set_id: ViewSetId,
        view_ref: &str,
    ) -> Option<&ViewBinding> {
        let attachment_id = self.view_set_insertion_attachments.get(&view_set_id)?;
        self.binding_attachments
            .get(attachment_id)?
            .views
            .get(view_ref)
    }

    pub(crate) fn insertion_attachment_id(&self, view_set_id: ViewSetId) -> Option<&str> {
        self.view_set_insertion_attachments
            .get(&view_set_id)
            .map(String::as_str)
    }

    #[cfg(test)]
    pub(crate) fn stamp_instance_binding(
        &mut self,
        instance: RyeOsViewInstanceKey,
        attachment_id: &str,
    ) -> bool {
        let Some(retained) = self.binding_attachments.get(attachment_id) else {
            return false;
        };
        let Some(presentation) = self.presentation.as_ref() else {
            return false;
        };
        let Some(super::presentation_state::MountRef::SurfaceTile {
            surface_id,
            view_set_id,
            tile_id,
            tile,
            ..
        }) = presentation.mount(&instance)
        else {
            return false;
        };
        if !retained.views.contains_key(&tile.view.view_ref) {
            return false;
        }
        let surface_id = surface_id.clone();
        let coordinate = super::binding::UiBindingAttachmentCoordinate {
            binding_attachment_id: attachment_id.into(),
            binding_generation: retained.descriptor.binding_generation,
            binding_digest: retained.descriptor.binding_digest.clone(),
        };
        self.presentation
            .as_mut()
            .expect("checked")
            .mutate_view_set(
                &surface_id,
                view_set_id,
                BTreeMap::from([(tile_id, coordinate)]),
                |_| Ok(()),
            )
            .is_ok()
    }

    /// Establish an explicit insertion default, not a per-mount binding fallback.
    #[cfg(test)]
    pub(crate) fn set_view_set_insertion_attachment(
        &mut self,
        view_set_id: ViewSetId,
        attachment_id: &str,
    ) -> bool {
        if self.view_set(view_set_id).is_none()
            || !self.binding_attachments.contains_key(attachment_id)
        {
            return false;
        }
        self.view_set_insertion_attachments
            .insert(view_set_id, attachment_id.into());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mounted_binding_lookup_cannot_borrow_a_sibling_view_from_its_closure() {
        let session = crate::ui::reducer::test_support::session_with_surface(serde_json::json!({
            "name":"fixture", "tiles":["view:test/one"],
            "views":{
                "view:test/one":{"widget":"rows"},
                "view:test/two":{"widget":"rows"}
            }
        }));
        let core = crate::ui::reducer::test_support::core(session);
        let mount = &core
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key;
        assert!(core.binding_for_instance(mount, "view:test/one").is_some());
        assert!(
            core.binding_context_for_instance(mount)
                .unwrap()
                .views
                .contains_key("view:test/two")
        );
        assert!(core.binding_for_instance(mount, "view:test/two").is_none());
        assert!(
            core.compiled_binding_operation(
                mount,
                super::super::binding::UiBindingCoordinate::Source {
                    view_ref: "view:test/two".into(),
                    channel: "default".into(),
                },
                super::super::binding::UiBindingPayload::SourceParameters {
                    params: serde_json::json!({}),
                }
            )
            .is_none()
        );
        let shared = core
            .presentation
            .as_ref()
            .unwrap()
            .dialogs()
            .values()
            .next()
            .unwrap()
            .view()
            .instance_key();
        assert!(
            core.compiled_binding_operation(
                shared,
                super::super::binding::UiBindingCoordinate::SurfaceRoute,
                super::super::binding::UiBindingPayload::Input {
                    value: "test".into(),
                    route: None
                }
            )
            .is_none(),
            "Shared Config never borrows an active Surface route"
        );
    }

    #[test]
    fn shared_root_decodes_views_without_surface_route_or_sources() {
        let descriptor: UiBindingAttachment = serde_json::from_value(serde_json::json!({
            "binding_attachment_id": "shared-one", "binding_generation": 1,
            "binding_digest": "digest", "root_ref": "config:ryeos/presentation",
            "root_generation": "generation",
            "presentation": {
                "definition": {"kind": "shared_presentation", "config": {
                    "category": "ui", "shared_presentation": {
                        "schema": 1, "title": "RyeOS", "launcher": "launch",
                        "dialogs": [{"id": "launch", "title": "Launch", "view": "view:ryeos/launcher"}],
                        "capabilities": {"sources": [], "affordances": []}
                    }
                }},
                "views": {"view:ryeos/launcher": {}}
            },
            "project_path": null, "subject":{"kind":"projectless"}, "posture": "observation_only",
            "binding_request_bounds": {"max_request_bytes": 4096, "max_input_bytes": 1024,"max_seat_append_bytes":65536}
        })).unwrap();
        let retained = RetainedUiBindingAttachment::from_descriptor(descriptor).unwrap();
        assert!(retained.surface_sources.is_empty());
        assert_eq!(
            retained.initial_input_route,
            super::super::seat::InputRoute::default()
        );
        assert!(retained.views.contains_key("view:ryeos/launcher"));
        assert_eq!(retained.descriptor.root_ref, "config:ryeos/presentation");
    }
}
