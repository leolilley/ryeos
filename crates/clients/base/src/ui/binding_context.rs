//! Exact retained binding ownership for mounted view instances.

use std::collections::BTreeMap;

use super::binding::UiBindingAttachment;
use super::content::{SourceBinding, ViewBinding};
use super::model::{RyeOsCore, RyeOsDockEdge, dock_view_instance_key};
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
        crate::surface::view_sets::validate_effective_view_sets(&descriptor.effective_surface)
            .map_err(<serde_json::Error as serde::de::Error>::custom)?;
        let surface: crate::surface::SurfaceSpec =
            serde_json::from_value(descriptor.effective_surface.clone())?;
        Ok(Self {
            initial_input_route: super::seat::InputRoute::from_surface_input(
                surface.input.as_ref(),
            )
            .unwrap_or_default(),
            surface_sources: surface.sources,
            views: super::content::views_from_surface(Some(&descriptor.effective_surface)),
            descriptor,
        })
    }
}

impl RyeOsCore {
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
        self.instance_binding_attachments
            .get(instance)
            .and_then(|id| self.binding_attachments.get(id))
    }

    pub(crate) fn binding_for_instance(
        &self,
        instance: &RyeOsViewInstanceKey,
        view_ref: &str,
    ) -> Option<&ViewBinding> {
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

    pub(crate) fn stamp_instance_binding(
        &mut self,
        instance: RyeOsViewInstanceKey,
        attachment_id: &str,
    ) -> bool {
        if !self.binding_attachments.contains_key(attachment_id) {
            return false;
        }
        self.instance_binding_attachments
            .insert(instance, attachment_id.to_string());
        true
    }

    pub(crate) fn stamp_view_set_mounts(
        &mut self,
        view_set_index: usize,
        attachment_id: &str,
    ) -> bool {
        if !self.binding_attachments.contains_key(attachment_id) {
            return false;
        }
        let Some(view_set) = self.view_sets.get(view_set_index) else {
            return false;
        };
        let view_set_id = view_set.id;
        let mut instances = view_set
            .tiles
            .values()
            .map(|tile| tile.instance_key.clone())
            .collect::<Vec<_>>();
        for edge in [
            RyeOsDockEdge::Top,
            RyeOsDockEdge::Bottom,
            RyeOsDockEdge::Left,
            RyeOsDockEdge::Right,
        ] {
            if view_set.docks.slot(edge).is_some() {
                instances.push(dock_view_instance_key(view_set_id, edge));
            }
        }
        self.view_set_insertion_attachments
            .insert(view_set_id, attachment_id.to_string());
        for instance in instances {
            self.instance_binding_attachments
                .insert(instance, attachment_id.to_string());
        }
        true
    }
}
