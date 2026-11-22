//! Exact return data for one unchanged mounted lens coordinate.
//! A frame restores that mount's subject/input route, never arbitrary seat
//! facet keys, draft text, grants, effects or a renderer's ambient focus.
use super::{attachment::SelectionAttachment, binding::UiBindingAttachmentCoordinate};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) fn required_route<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Option::<Value>::deserialize(deserializer)
}
pub(crate) fn required_label<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedLensFrame {
    pub binding: UiBindingAttachmentCoordinate,
    pub view_ref: String,
    pub attachment: SelectionAttachment,
    /// Object or explicit null. Causal fold supplies the prior mount's exact
    /// route; shape validation alone does not admit or interpret route values.
    #[serde(deserialize_with = "required_route")]
    pub input_route: Option<Value>,
    #[serde(deserialize_with = "required_label")]
    pub label: Option<String>,
}
impl RetainedLensFrame {
    pub fn validate_shape(&self, max_bytes: u64) -> Result<(), String> {
        super::composition_operation::binding(&self.binding)?;
        super::composition_operation::view(&self.view_ref)?;
        if self
            .input_route
            .as_ref()
            .is_some_and(|value| !value.is_object())
        {
            return Err("lens input route must be an object or explicit null".into());
        }
        if self.label.as_ref().is_some_and(|label| {
            label.is_empty()
                || label.trim() != label
                || label.chars().any(char::is_control)
                || label.len() > crate::surface::view_sets::MAX_VIEW_SET_LABEL_BYTES
        }) {
            return Err("invalid lens label".into());
        }
        match &self.attachment {
            SelectionAttachment::FollowViewSet { view_set_id } if view_set_id.0 == 0 => {
                return Err("lens subject has zero tab identity".into());
            }
            SelectionAttachment::Pinned { fingerprint, .. }
                if fingerprint.is_empty() || fingerprint.chars().any(char::is_control) =>
            {
                return Err("lens pin fingerprint is incomplete".into());
            }
            SelectionAttachment::RequiredSubject { input, facets }
                if input.is_empty()
                    || input.trim() != input
                    || input.chars().any(char::is_control)
                    || facets.is_empty() =>
            {
                return Err("lens required subject shape is incomplete".into());
            }
            _ => {}
        }
        let bytes = serde_json::to_vec(self).map_err(|error| error.to_string())?;
        if max_bytes == 0 || bytes.len() as u64 > max_bytes {
            return Err("lens return data exceeds exact admitted bound".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn frame() -> RetainedLensFrame {
        RetainedLensFrame {
            binding: UiBindingAttachmentCoordinate {
                binding_attachment_id: "binding:test".into(),
                binding_generation: 1,
                binding_digest: "ab".repeat(32),
            },
            view_ref: "view:test/lens".into(),
            attachment: SelectionAttachment::FollowViewSet {
                view_set_id: crate::ids::ViewSetId::new(1),
            },
            input_route: None,
            label: None,
        }
    }
    #[test]
    fn nullable_fields_are_required_and_frame_has_no_global_facet_restore() {
        let value = serde_json::to_value(frame()).unwrap();
        for key in ["input_route", "label"] {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(serde_json::from_value::<RetainedLensFrame>(missing).is_err());
        }
        let mut forbidden = value;
        forbidden["facets"] = json!({"global":true});
        assert!(serde_json::from_value::<RetainedLensFrame>(forbidden).is_err());
    }
    #[test]
    fn exact_bound_route_shape_and_label_are_checked() {
        let original = frame();
        let bytes = serde_json::to_vec(&original).unwrap().len() as u64;
        original.validate_shape(bytes).unwrap();
        assert!(original.validate_shape(bytes - 1).is_err());
        let mut bad = original.clone();
        bad.input_route = Some(json!("global.input"));
        assert!(bad.validate_shape(65536).is_err());
        bad = original.clone();
        bad.label = Some(" bad ".into());
        assert!(bad.validate_shape(65536).is_err());
        bad = original;
        bad.input_route = Some(json!({"thread":"retained-thread"}));
        bad.validate_shape(65536).unwrap();
    }
}
