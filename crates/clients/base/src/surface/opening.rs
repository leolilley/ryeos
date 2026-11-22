//! Ordinary Surface-opening data, never compilation or publication authority.
use super::saved_surfaces::ReusableSurfaceRecord;
use super::view_sets::SavedViewSelectionSource;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SurfaceOpenSelection {
    Authored {
        surface_ref: String,
    },
    Reusable {
        id: String,
        expected_revision: u64,
        fresh_subjects: BTreeMap<String, BTreeMap<String, BTreeMap<String, Value>>>,
    },
    Composed(ComposedSurfaceSelection),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposedSurfaceSelection {
    pub origin: super::saved_surfaces::SavedSurfaceOrigin,
    pub title: String,
    pub active_view_set_id: String,
    pub tabs: Vec<ComposedSurfaceTab>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposedSurfaceTab {
    pub origin: super::saved_surfaces::SavedSurfaceOrigin,
    pub template: super::view_sets::SavedViewSetTemplate,
    pub fresh_subjects: BTreeMap<String, BTreeMap<String, Value>>,
}

impl ComposedSurfaceSelection {
    pub fn validate(&self) -> Result<(), String> {
        use super::view_sets::*;
        let valid_origin = |origin: &super::saved_surfaces::SavedSurfaceOrigin| {
            origin
                .surface_ref
                .strip_prefix("surface:")
                .is_some_and(|value| {
                    !value.is_empty()
                        && value.trim() == value
                        && !value.chars().any(char::is_control)
                })
                && origin.project.as_ref().is_none_or(|project| {
                    !project.local_id.is_empty()
                        && project.local_id.len() <= MAX_PROJECT_LOCAL_ID_BYTES
                        && project.local_id.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':')
                        })
                })
        };
        if !valid_origin(&self.origin)
            || self.title.trim().is_empty()
            || self.title.len() > MAX_VIEW_SET_LABEL_BYTES
            || self.title.chars().any(char::is_control)
            || self.tabs.is_empty()
            || self.tabs.len() > MAX_VIEW_SETS
        {
            return Err("composed Surface origin, title or tab count is invalid".into());
        }
        let templates = self
            .tabs
            .iter()
            .map(|tab| tab.template.clone())
            .collect::<Vec<_>>();
        validate_saved_view_set_templates(&templates)?;
        let ids = templates
            .iter()
            .map(|template| template.id.as_str())
            .collect::<BTreeSet<_>>();
        if !ids.contains(self.active_view_set_id.as_str()) {
            return Err("composed Surface active tab is absent".into());
        }
        for tab in &self.tabs {
            if !valid_origin(&tab.origin) {
                return Err("composed tab origin is invalid".into());
            }
            let required = tab
                .template
                .relationships
                .iter()
                .filter_map(|relation| match &relation.source {
                    SavedViewSelectionSource::RequiredSubject { input, facets } => {
                        Some((input.as_str(), facets))
                    }
                    _ => None,
                })
                .collect::<BTreeMap<_, _>>();
            for (input, values) in &tab.fresh_subjects {
                let facets = required
                    .get(input.as_str())
                    .ok_or("composed tab supplies an unused subject")?;
                if facets.len() != values.len()
                    || facets
                        .iter()
                        .any(|facet| values.get(facet).is_none_or(Value::is_null))
                {
                    return Err("composed tab fresh subject contradicts its contract".into());
                }
            }
            if tab.template.relationships.iter().any(|relation|
                matches!(&relation.source,SavedViewSelectionSource::FollowSet{saved_view_set_id} if !ids.contains(saved_view_set_id.as_str()))) {
                return Err("composed follower leaves its collection".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SurfaceOpenPlacement {
    NewSurface,
    NewPresentation {
        root_binding: crate::ui::binding::UiBindingAttachmentCoordinate,
    },
    AppendTabs {
        destination_surface_instance_id: crate::ids::SurfaceInstanceId,
        destination_binding: crate::ui::binding::UiBindingAttachmentCoordinate,
        expected_composition_revision: u64,
        expected_next_mount_id: u64,
        insertion_index: usize,
        activate_imported_tab: bool,
    },
}

impl SurfaceOpenPlacement {
    /// Shared lowering used after signed parameters have been resolved. A
    /// browser-supplied purpose never substitutes for this closed request.
    pub fn purpose(&self) -> crate::ui::admission_operation::UiPresentationOperationKind {
        use crate::ui::admission_operation::UiPresentationOperationKind;
        match self {
            Self::NewSurface => UiPresentationOperationKind::OpenSurface,
            Self::NewPresentation { .. } => UiPresentationOperationKind::OpenPresentationDocument,
            Self::AppendTabs { .. } => UiPresentationOperationKind::ImportSurfaceTabs,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceOpenRequest {
    pub creation_request_id: String,
    pub selection: SurfaceOpenSelection,
    pub placement: SurfaceOpenPlacement,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceOpenTarget {
    pub selection: SurfaceOpenSelection,
    pub placement: SurfaceOpenPlacement,
}
impl SurfaceOpenTarget {
    pub fn from_resolved_params(params: &Value) -> Result<Self, String> {
        let fields = params
            .as_object()
            .ok_or("Surface opening parameters must be an object")?;
        if fields.keys().any(|field| {
            !matches!(
                field.as_str(),
                "selection" | "placement" | "creation_request_id" | "local_id"
            )
        }) {
            return Err("unknown Surface opening parameter".into());
        }
        Ok(Self {
            selection: serde_json::from_value(
                params
                    .get("selection")
                    .cloned()
                    .ok_or("Surface selection absent")?,
            )
            .map_err(|error| error.to_string())?,
            placement: serde_json::from_value(
                params
                    .get("placement")
                    .cloned()
                    .ok_or("Surface placement absent")?,
            )
            .map_err(|error| error.to_string())?,
        })
    }
}
impl SurfaceOpenRequest {
    pub fn from_resolved_params(params: &Value) -> Result<Self, String> {
        let target = SurfaceOpenTarget::from_resolved_params(params)?;
        let request = Self {
            creation_request_id: params
                .get("creation_request_id")
                .and_then(Value::as_str)
                .ok_or("Surface creation identity absent")?
                .into(),
            selection: target.selection,
            placement: target.placement,
        };
        request.validate()?;
        Ok(request)
    }
    pub fn target(&self) -> SurfaceOpenTarget {
        SurfaceOpenTarget {
            selection: self.selection.clone(),
            placement: self.placement.clone(),
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        if !crate::ui::admission_operation::canonical_operation_id(&self.creation_request_id) {
            return Err("Surface creation identity is not a canonical UUID".into());
        }
        if let SurfaceOpenPlacement::NewPresentation { root_binding } = &self.placement {
            crate::ui::composition_operation::binding(root_binding)?;
        }
        if let SurfaceOpenPlacement::AppendTabs {
            destination_binding,
            expected_next_mount_id,
            insertion_index,
            ..
        } = &self.placement
        {
            crate::ui::composition_operation::binding(destination_binding)?;
            if *expected_next_mount_id == 0 || *insertion_index > super::view_sets::MAX_VIEW_SETS {
                return Err("Surface import placement is invalid".into());
            }
        }
        match &self.selection {
            SurfaceOpenSelection::Authored { surface_ref } => {
                if !surface_ref
                    .strip_prefix("surface:")
                    .is_some_and(|reference| {
                        !reference.is_empty()
                            && reference.trim() == reference
                            && !reference.chars().any(char::is_control)
                    })
                {
                    return Err("authored Surface reference is invalid".into());
                }
            }
            SurfaceOpenSelection::Reusable { id, .. } => {
                if id.is_empty()
                    || id.len() > super::view_sets::MAX_SAVED_VIEW_SET_ID_BYTES
                    || !id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                {
                    return Err("reusable Surface identity is invalid".into());
                }
            }
            SurfaceOpenSelection::Composed(composition) => composition.validate()?,
        }
        Ok(())
    }

    /// Check keys against the exact record. Missing inputs stay unresolved;
    /// supplied facets, values and bounds remain the canonical subject owner's
    /// responsibility after fresh descriptor admission.
    pub fn validate_reusable(&self, record: &ReusableSurfaceRecord) -> Result<(), String> {
        self.validate()?;
        super::saved_surfaces::validate_saved_surface_libraries(
            std::slice::from_ref(record),
            &[],
            &[],
        )?;
        let SurfaceOpenSelection::Reusable {
            id, fresh_subjects, ..
        } = &self.selection
        else {
            return Err("authored opening cannot use a reusable record".into());
        };
        if id != &record.id {
            return Err("opening selects a different reusable record".into());
        }
        for (tab_id, inputs) in fresh_subjects {
            let tab = record
                .view_sets
                .iter()
                .find(|tab| &tab.id == tab_id)
                .ok_or("fresh subjects name an unknown reusable tab")?;
            let required = tab
                .relationships
                .iter()
                .filter_map(|relationship| match &relationship.source {
                    SavedViewSelectionSource::RequiredSubject { input, .. } => Some(input.as_str()),
                    _ => None,
                })
                .collect::<BTreeSet<_>>();
            if inputs
                .keys()
                .any(|input| !required.contains(input.as_str()))
            {
                return Err("fresh subjects name an unused reusable input".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn record() -> ReusableSurfaceRecord {
        serde_json::from_value(json!({
            "id":"solver","title":"Solver","origin":{"surface_ref":"surface:test/solver"},
            "active_view_set_id":"run","tab_origins":{"run":{"surface_ref":"surface:test/run"}},
            "view_sets":[{"id":"run","name":"Run","composition":{"schema":2,"id":"run","title":"Run","root":{"type":"group","views":["view:test/run"],"active":0}},
                "relationships":[{"mount":{"kind":"tile","index":0},"source":{"mode":"required_subject","input":"run","facets":["selection.work"]}}]}]
        })).unwrap()
    }
    fn request() -> SurfaceOpenRequest {
        serde_json::from_value(json!({"creation_request_id":"11111111-1111-4111-8111-111111111111",
            "selection":{"kind":"reusable","id":"solver","expected_revision":0,"fresh_subjects":{}},"placement":{"kind":"new_surface"}})).unwrap()
    }
    #[test]
    fn resolved_wrapper_is_closed_and_import_uses_exact_binding_validator() {
        let mut params = serde_json::to_value(request()).unwrap();
        params["local_id"] = json!("registered-project");
        assert!(SurfaceOpenRequest::from_resolved_params(&params).is_ok());
        params["ambient_authority"] = json!(true);
        assert!(SurfaceOpenRequest::from_resolved_params(&params).is_err());
        let mut value = request();
        value.placement = SurfaceOpenPlacement::AppendTabs {
            destination_surface_instance_id: crate::ids::SurfaceInstanceId::try_new(
                "22222222-2222-4222-8222-222222222222",
            )
            .unwrap(),
            destination_binding: crate::ui::binding::UiBindingAttachmentCoordinate {
                binding_attachment_id: "exact\nother".into(),
                binding_generation: 1,
                binding_digest: "digest".into(),
            },
            expected_composition_revision: 0,
            expected_next_mount_id: 1,
            insertion_index: 0,
            activate_imported_tab: false,
        };
        assert!(value.validate().is_err());
        let SurfaceOpenPlacement::AppendTabs {
            destination_binding,
            ..
        } = &mut value.placement
        else {
            unreachable!()
        };
        destination_binding.binding_attachment_id = "exact".into();
        destination_binding.binding_digest = "digest\nother".into();
        assert!(value.validate().is_err());
    }
    #[test]
    fn missing_subjects_stay_unresolved_and_unknown_keys_refuse() {
        let record = record();
        let mut request = request();
        request.validate_reusable(&record).unwrap();
        if let SurfaceOpenSelection::Reusable { fresh_subjects, .. } = &mut request.selection {
            fresh_subjects.insert("other".into(), BTreeMap::new());
        }
        assert!(request.validate_reusable(&record).is_err());
        if let SurfaceOpenSelection::Reusable { fresh_subjects, .. } = &mut request.selection {
            fresh_subjects.clear();
            fresh_subjects.insert(
                "run".into(),
                BTreeMap::from([("unused".into(), BTreeMap::new())]),
            );
        }
        assert!(request.validate_reusable(&record).is_err());
    }
    #[test]
    fn closed_request_has_no_predecessor_defaults() {
        let value = serde_json::to_value(request()).unwrap();
        for key in ["expected_revision", "fresh_subjects"] {
            let mut missing = value.clone();
            missing["selection"].as_object_mut().unwrap().remove(key);
            assert!(serde_json::from_value::<SurfaceOpenRequest>(missing).is_err());
        }
        let mut extra = value;
        extra["selection"]["surface_ref"] = json!("surface:test/other");
        assert!(serde_json::from_value::<SurfaceOpenRequest>(extra).is_err());
        let mut invalid = request();
        invalid.creation_request_id = "not-an-operation".into();
        assert!(invalid.validate().is_err());
    }
}
