//! Runtime subject attachment for mounted views.
//!
//! Attachments are presentation/session state keyed by mounted instance. They
//! are deliberately outside reusable layout preferences. A pinned observation
//! is not portable authority; reusable compositions retain only relationship
//! shape and may mount a required subject as an explicit execution fence.

use super::content::ViewBinding;
use super::model::RyeOsCore;
use crate::ids::{RyeOsViewInstanceKey, ViewSetId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum SelectionAttachment {
    FollowViewSet {
        view_set_id: ViewSetId,
    },
    Pinned {
        values: BTreeMap<String, Value>,
        fingerprint: String,
    },
    /// A reusable composition retained the shape of a subject relationship,
    /// but deliberately retained none of its values or authority.  This is an
    /// engine execution fence, not an empty selection.
    RequiredSubject {
        input: String,
        facets: Vec<String>,
    },
}

fn is_selection_facet(path: &str) -> bool {
    path == super::seat::KEY_SELECTION
        || path.starts_with(&format!("{}.", super::seat::KEY_SELECTION))
}

fn collect_facet_dependencies(value: &Value, dependencies: &mut BTreeSet<String>) {
    match value {
        Value::String(value) => {
            if let Some(path) = value.strip_prefix("@facet:")
                && let Some(path) = path.split('|').next()
            {
                dependencies.insert(path.to_string());
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_facet_dependencies(value, dependencies);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                collect_facet_dependencies(value, dependencies);
            }
        }
        _ => {}
    }
}

/// Logical selection values a binding reads. Refresh declarations and UI
/// writes are subscriptions/actions, not values that can define a pin.
/// Admission and mounted restoration use this same grammar; it describes
/// dependencies only and does not admit or resolve any supplied subject.
pub fn selection_dependencies(binding: &ViewBinding) -> BTreeSet<String> {
    facet_dependencies(binding)
        .into_iter()
        .filter(|path| is_selection_facet(path))
        .collect()
}

/// Construct bounded pin metadata after its caller has validated the exact
/// subject/dependency relationship. This function admits no values or grants.
pub(crate) fn bounded_subject_pin(
    values: BTreeMap<String, Value>,
    byte_limit: u64,
) -> Result<SelectionAttachment, String> {
    let encoded = serde_json::to_vec(&values).map_err(|error| error.to_string())?;
    if byte_limit == 0 {
        return Err("active session request byte bound is zero".into());
    }
    let byte_limit = usize::try_from(byte_limit)
        .map_err(|_| "active session request byte bound exceeds this platform")?;
    if encoded.len() > byte_limit {
        return Err(format!(
            "pinned selection is {} bytes (max {byte_limit})",
            encoded.len()
        ));
    }
    use sha2::{Digest, Sha256};
    Ok(SelectionAttachment::Pinned {
        values,
        fingerprint: format!("{:x}", Sha256::digest(&encoded)),
    })
}

/// Logical facet values a binding reads. Lens return frames use this broader
/// set so they restore the originating route as well as selection, without
/// rolling back unrelated seat state.
pub(crate) fn facet_dependencies(binding: &ViewBinding) -> BTreeSet<String> {
    let mut dependencies = BTreeSet::new();
    collect_facet_dependencies(&binding.body, &mut dependencies);
    for source in binding.sources.values() {
        collect_facet_dependencies(&source.params, &mut dependencies);
    }
    if let Some(facet) = binding.facet.as_deref() {
        dependencies.insert(facet.to_string());
    }
    if let Some(field_state) = binding.field_state.as_ref() {
        for subject in &field_state.cursor_scope.subject {
            collect_facet_dependencies(&Value::String(subject.clone()), &mut dependencies);
        }
    }
    dependencies
}

fn refresh_uses_selection(refresh: &Value) -> bool {
    refresh
        .get("on_facet")
        .and_then(Value::as_str)
        .is_some_and(is_selection_facet)
}

/// Whether this mounted binding reads, writes, or subscribes to selection.
/// Pinning is deliberately narrower (`selection_dependencies`), but placement
/// must retain the owner for every binding whose behavior is selection-scoped.
pub(crate) fn participates_in_selection(binding: &ViewBinding) -> bool {
    !selection_dependencies(binding).is_empty()
        || refresh_uses_selection(&binding.refresh)
        || binding
            .sources
            .values()
            .any(|source| refresh_uses_selection(&source.refresh))
        || binding.affordances.iter().any(|affordance| {
            let Some(invoke) = affordance.get("invoke") else {
                return false;
            };
            invoke.get("plane").and_then(Value::as_str) == Some("ui")
                && invoke
                    .get("facet")
                    .and_then(Value::as_str)
                    .is_some_and(is_selection_facet)
        })
}

impl RyeOsCore {
    pub(crate) fn selection_attachment_for_instance(
        &self,
        instance: &RyeOsViewInstanceKey,
    ) -> Option<SelectionAttachment> {
        self.mount_owner(instance)?;
        // A selection-independent View has no semantic follower. Replay may
        // retain its original default FollowViewSet after placement, but that
        // address must not become a dependency or a saved FollowSet edge.
        if self
            .mounted_view_ref(instance)
            .and_then(|view_ref| self.binding_for_instance(instance, view_ref))
            .is_some_and(|binding| !participates_in_selection(binding))
        {
            return self.view_set_for_instance(instance).map(|set| {
                SelectionAttachment::FollowViewSet {
                    view_set_id: set.id,
                }
            });
        }
        if let Some(attachment) = self.selection_attachments.get(instance) {
            if let SelectionAttachment::FollowViewSet { view_set_id } = attachment
                && !self.same_surface(instance, *view_set_id)
            {
                return None;
            }
            return Some(attachment.clone());
        }
        let view_set = self.view_set_for_instance(instance)?;
        Some(SelectionAttachment::FollowViewSet {
            view_set_id: view_set.id,
        })
    }

    pub(crate) fn followed_selection_view_set(
        &self,
        instance: &RyeOsViewInstanceKey,
    ) -> Option<ViewSetId> {
        match self.selection_attachment_for_instance(instance) {
            Some(SelectionAttachment::FollowViewSet { view_set_id }) => self
                .same_surface(instance, view_set_id)
                .then_some(view_set_id),
            Some(SelectionAttachment::Pinned { .. }) => None,
            Some(SelectionAttachment::RequiredSubject { .. }) => None,
            None => None,
        }
    }

    /// Resolve one exact facet key. `resolve_params` owns dotted-field
    /// fallback and calls this repeatedly with progressively shorter keys.
    pub(crate) fn facet_value_for_instance(
        &self,
        instance: &RyeOsViewInstanceKey,
        logical_facet: &str,
    ) -> Option<Value> {
        self.mount_owner(instance)?;
        if !is_selection_facet(logical_facet) {
            return self.seat.fold().get(logical_facet).cloned();
        }
        match self.selection_attachment_for_instance(instance) {
            Some(SelectionAttachment::Pinned { values, .. }) => values.get(logical_facet).cloned(),
            Some(SelectionAttachment::RequiredSubject { .. }) => None,
            Some(SelectionAttachment::FollowViewSet { .. }) | None => {
                let view_set_id = self.followed_selection_view_set(instance)?;
                self.selection_value_in_view_set(view_set_id, logical_facet)
            }
        }
    }

    /// A retained saved-tab subject is an admitted bootstrap seed, not a
    /// synthetic seat event. Durable writes (including explicit null) win.
    pub(crate) fn selection_value_in_view_set(
        &self,
        view_set_id: ViewSetId,
        logical_facet: &str,
    ) -> Option<Value> {
        self.view_set(view_set_id)?;
        let key = super::seat::selection_storage_key(view_set_id, logical_facet)?;
        if let Some(value) = self.seat.fold().get(&key) {
            return Some(value.clone());
        }
        if logical_facet != "selection.work" {
            return None;
        }
        if let Some(work) = self.retained_work_selections.get(&view_set_id) {
            return Some(work.clone());
        }
        let presentation = self.presentation.as_ref()?;
        let (surface_id, _) = presentation
            .view_sets()
            .find(|(_, set)| set.id == view_set_id)?;
        let work = presentation
            .surfaces()
            .get(surface_id)?
            .admitted_saved_tab(view_set_id)?
            .particular()?
            .resolved
            .resolved_selection_work
            .as_ref()?;
        Some(serde_json::json!({"thread":work.thread,"chain_root":work.chain_root}))
    }

    pub(crate) fn unresolved_required_subject(
        &self,
        instance: &RyeOsViewInstanceKey,
    ) -> Option<(String, Vec<String>)> {
        match self.selection_attachments.get(instance) {
            Some(SelectionAttachment::RequiredSubject { input, facets }) => {
                Some((input.clone(), facets.clone()))
            }
            _ => None,
        }
    }

    pub(crate) fn instance_has_unresolved_required_subject(
        &self,
        instance: &RyeOsViewInstanceKey,
    ) -> bool {
        matches!(
            self.selection_attachments.get(instance),
            Some(SelectionAttachment::RequiredSubject { .. })
        )
    }

    pub(crate) fn subject_values_from_view_set(
        &self,
        view_set_id: ViewSetId,
        facets: &[String],
    ) -> Option<BTreeMap<String, Value>> {
        self.view_set(view_set_id)?;
        let mut values = BTreeMap::new();
        for facet in facets {
            let reference = Value::String(format!("@facet:{facet}"));
            let value = super::content::resolve_params(&reference, |candidate| {
                self.selection_value_in_view_set(view_set_id, candidate)
            });
            if value.is_null() {
                return None;
            }
            values.insert(facet.clone(), value);
        }
        Some(values)
    }

    pub(crate) fn bounded_pinned_subject(
        &self,
        instance: &RyeOsViewInstanceKey,
        values: BTreeMap<String, Value>,
    ) -> Result<SelectionAttachment, String> {
        let byte_limit = self
            .binding_attachment_for_instance(instance)
            .ok_or("active session request bounds are unavailable")?
            .binding_request_bounds
            .max_request_bytes;
        bounded_subject_pin(values, byte_limit)
    }

    /// Capture only exact selection dependencies read by the mounted binding. The
    /// values come from the current effective attachment, never from a client
    /// payload, and remain bounded runtime state.
    pub(crate) fn capture_pinned_selection(
        &self,
        instance: &RyeOsViewInstanceKey,
    ) -> Result<SelectionAttachment, String> {
        if self.instance_has_unresolved_required_subject(instance) {
            return Err("required subject has not been supplied".into());
        }
        let view_ref = self
            .mounted_view_ref(instance)
            .ok_or("view instance is not mounted")?;
        let binding = self
            .binding_for_instance(instance, view_ref)
            .ok_or("view binding is unavailable")?;
        let dependencies = selection_dependencies(binding);
        if dependencies.is_empty() {
            return Err("view has no readable selection dependency to pin".into());
        }
        let mut values = BTreeMap::new();
        for dependency in dependencies {
            let reference = Value::String(format!("@facet:{dependency}"));
            let value = super::content::resolve_params(&reference, |candidate| {
                self.facet_value_for_instance(instance, candidate)
            });
            // Keep unresolved/null dependencies explicitly: a later selection
            // cannot fill them, and admission checks this exact dependency set.
            values.insert(dependency, value);
        }
        self.bounded_pinned_subject(instance, values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::reducer::test_support::*;

    #[test]
    fn pure_pin_constructor_uses_the_exact_admitted_byte_boundary() {
        let values = BTreeMap::from([(
            "selection.work".into(),
            serde_json::json!({"thread":"T-one"}),
        )]);
        let bytes = serde_json::to_vec(&values).unwrap();
        assert!(bounded_subject_pin(values.clone(), 0).is_err());
        assert!(bounded_subject_pin(values.clone(), bytes.len() as u64 - 1).is_err());
        let exact = bounded_subject_pin(values.clone(), bytes.len() as u64).unwrap();
        assert_eq!(
            exact,
            bounded_subject_pin(values, bytes.len() as u64 + 1).unwrap()
        );
        let SelectionAttachment::Pinned { fingerprint, .. } = exact else {
            panic!("bounded constructor returns pin metadata only");
        };
        assert_eq!(fingerprint.len(), 64);
    }

    fn mounted_selection_view() -> (RyeOsCore, RyeOsViewInstanceKey) {
        // Pin capture is a mounted-subject test, not an insertion test. Admit
        // the exact selected View in the initial manifest before bootstrapping.
        let core = core(session_with_surface(serde_json::json!({
            "name":"Selection", "tiles":["view:test/selection"],
            "views":{"view:test/selection":{
                "widget": "rows",
                "sources": {"default": {
                    "ref": "service:test/source",
                    "params": {"thread": "@facet:selection.work.thread"}
                }}
            }}
        })));
        let tile = core.active_view_set().unwrap().focused_tile;
        let instance = core.active_view_set().unwrap().tiles[&tile]
            .instance_key
            .clone();
        (core, instance)
    }

    #[test]
    fn absent_attachment_follows_the_containing_view_set() {
        let (mut core, instance) = mounted_selection_view();
        let owner = core.active_view_set().unwrap().id;
        let key = super::super::seat::selection_storage_key(owner, "selection.work").unwrap();
        core.seat
            .append_facet(key, serde_json::json!({"thread": "T-one"}));

        assert_eq!(
            core.selection_attachment_for_instance(&instance),
            Some(SelectionAttachment::FollowViewSet { view_set_id: owner })
        );
        assert_eq!(
            core.facet_value_for_instance(&instance, "selection.work"),
            Some(serde_json::json!({"thread": "T-one"}))
        );
    }

    #[test]
    fn pinned_capture_is_exact_and_independent_of_later_selection() {
        let (mut core, instance) = mounted_selection_view();
        let owner = core.active_view_set().unwrap().id;
        let key = super::super::seat::selection_storage_key(owner, "selection.work").unwrap();
        core.seat.append_facet(
            key.clone(),
            serde_json::json!({"thread": "T-one", "unrelated":"not captured"}),
        );
        let pinned = core.capture_pinned_selection(&instance).unwrap();
        let SelectionAttachment::Pinned { values, .. } = &pinned else {
            panic!("capture returns pinned values");
        };
        assert_eq!(
            values,
            &BTreeMap::from([("selection.work.thread".into(), serde_json::json!("T-one"))]),
            "capture uses exactly the declaration, never its broader parent object"
        );
        core.selection_attachments
            .insert(instance.clone(), pinned.clone());
        core.seat
            .append_facet(key, serde_json::json!({"thread": "T-two"}));

        assert_eq!(
            core.facet_value_for_instance(&instance, "selection.work.thread"),
            Some(serde_json::json!("T-one"))
        );
        assert!(matches!(pinned, SelectionAttachment::Pinned { .. }));
        assert!(core.followed_selection_view_set(&instance).is_none());
        // Plain layout preferences cannot silently discard a pinned subject;
        // reusable capture has its own explicit RequiredSubject contract.
        assert_eq!(
            core.export_layout_preferences().unwrap_err(),
            "layout preferences cannot preserve pinned or cross-set subject relationships"
        );
    }

    #[test]
    fn pin_refuses_writer_only_binding() {
        let core = core(session_with_surface(serde_json::json!({
            "name":"Writer", "tiles":["view:test/writer"],
            "views":{"view:test/writer":{
                "widget": "rows",
                "affordances": [{
                    "id": "select",
                    "invoke": {"plane": "ui", "facet": "selection", "value": {}}
                }]
            }}
        })));
        let tile = core.active_view_set().unwrap().focused_tile;
        let instance = core.active_view_set().unwrap().tiles[&tile]
            .instance_key
            .clone();

        assert_eq!(
            core.capture_pinned_selection(&instance).unwrap_err(),
            "view has no readable selection dependency to pin"
        );
        assert!(participates_in_selection(
            core.binding_for_insertion(core.active_view_set().unwrap().id, "view:test/writer")
                .unwrap()
        ));
    }

    #[test]
    fn unrelated_binding_has_no_selection_participation() {
        let mut core = core(session());
        seed_view(&mut core, "view:test/unrelated");
        assert!(!participates_in_selection(
            core.binding_for_insertion(core.active_view_set().unwrap().id, "view:test/unrelated")
                .unwrap()
        ));
    }

    #[test]
    fn contextual_dialog_explicit_follow_stays_with_its_own_surface() {
        let mut session =
            super::super::reducer::test_support::session_with_surface(serde_json::json!({
                "name":"Work", "tiles":["view:test/subject"],
                "dialogs":[{"id":"inspect","title":"Inspect","view":"view:test/inspect"}],
                "views":{
                    "view:test/subject":{"widget":"text","body":"Subject"},
                    "view:test/inspect":{"widget":"text","body":"@facet:selection.thread_id"}
                }
            }));
        let mut sibling = session.presentations[0].surfaces[0].clone();
        sibling.surface_instance_id = "00000000-0000-4000-8000-000000000099".into();
        sibling.ordinal = 2;
        sibling.mount_manifest =
            super::super::composition_identity::UiSurfaceMountManifest::allocate(
                crate::ids::PresentationId::try_new(
                    session.presentations[0].presentation_id.clone(),
                )
                .unwrap(),
                crate::ids::SurfaceInstanceId::try_new(sibling.surface_instance_id.clone())
                    .unwrap(),
                session.presentations[0].next_mount_id,
                sibling
                    .mount_manifest
                    .tabs
                    .iter()
                    .map(|tab| (tab.seed.clone(), tab.binding.clone()))
                    .collect(),
                sibling
                    .mount_manifest
                    .tabs
                    .first()
                    .map(|tab| tab.seed.id.as_str()),
            )
            .unwrap();
        session.presentations[0].next_mount_id = sibling.mount_manifest.next_mount_id;
        session.presentations[0].surfaces.push(sibling);
        let mut core = core(session);
        let composition = core.presentation.as_ref().unwrap();
        let mut surfaces = composition.ordered_surfaces();
        let first = surfaces.next().unwrap();
        let second = surfaces.next().unwrap();
        let owner_set = first.active_view_set().unwrap().id;
        let sibling_set = second.active_view_set().unwrap().id;
        let instance =
            RyeOsViewInstanceKey::surface_dialog(composition.id(), first.id(), "inspect").unwrap();
        assert!(
            core.view_set_for_instance(&instance).is_none(),
            "dialog must not invent a tab owner"
        );
        assert!(core.same_surface(&instance, owner_set));
        assert!(!core.same_surface(&instance, sibling_set));
        assert!(
            core.selection_attachment_for_instance(&instance).is_none(),
            "no implicit selection follow"
        );
        core.selection_attachments.insert(
            instance.clone(),
            SelectionAttachment::FollowViewSet {
                view_set_id: owner_set,
            },
        );
        core.seat.append_facet(
            super::super::seat::selection_storage_key(owner_set, "selection").unwrap(),
            serde_json::json!({"thread_id":"T-own"}),
        );
        assert_eq!(
            core.facet_value_for_instance(&instance, "selection"),
            Some(serde_json::json!({"thread_id":"T-own"}))
        );
        core.selection_attachments.insert(
            instance.clone(),
            SelectionAttachment::FollowViewSet {
                view_set_id: sibling_set,
            },
        );
        assert!(core.selection_attachment_for_instance(&instance).is_none());
        assert!(
            core.facet_value_for_instance(&instance, "selection")
                .is_none()
        );
    }

    #[test]
    fn pin_freezes_an_unresolved_declared_dependency_as_absent() {
        let (mut core, instance) = mounted_selection_view();
        let pinned = core.capture_pinned_selection(&instance).unwrap();
        core.selection_attachments.insert(instance.clone(), pinned);

        let owner = core.active_view_set().unwrap().id;
        let key = super::super::seat::selection_storage_key(owner, "selection.work").unwrap();
        core.seat
            .append_facet(key, serde_json::json!({"thread": "T-later"}));

        assert_eq!(
            core.facet_value_for_instance(&instance, "selection.work"),
            None
        );
        assert_eq!(
            core.facet_value_for_instance(&instance, "selection.work.thread"),
            Some(Value::Null),
            "the exact unresolved dependency remains explicitly absent"
        );
    }

    #[test]
    fn pin_requires_an_admitted_nonzero_request_bound() {
        let (mut core, instance) = mounted_selection_view();
        core.binding_attachments
            .get_mut("fixture-attachment")
            .unwrap()
            .descriptor
            .binding_request_bounds
            .max_request_bytes = 0;
        assert_eq!(
            core.capture_pinned_selection(&instance).unwrap_err(),
            "active session request byte bound is zero"
        );

        core.binding_attachments.remove("fixture-attachment");
        assert_eq!(
            core.capture_pinned_selection(&instance).unwrap_err(),
            "view binding is unavailable"
        );
    }

    #[test]
    fn pin_accepts_the_exact_byte_limit_and_rejects_one_byte_over() {
        let (mut core, instance) = mounted_selection_view();
        let owner = core.active_view_set().unwrap().id;
        let key = super::super::seat::selection_storage_key(owner, "selection.work").unwrap();
        core.seat
            .append_facet(key, serde_json::json!({"thread": "T-bounded"}));
        let SelectionAttachment::Pinned { values, .. } =
            core.capture_pinned_selection(&instance).unwrap()
        else {
            panic!("capture must produce a pin");
        };
        let encoded_len = serde_json::to_vec(&values).unwrap().len();

        core.binding_attachments
            .get_mut("fixture-attachment")
            .unwrap()
            .descriptor
            .binding_request_bounds
            .max_request_bytes = u64::try_from(encoded_len).unwrap();
        assert!(core.capture_pinned_selection(&instance).is_ok());

        core.binding_attachments
            .get_mut("fixture-attachment")
            .unwrap()
            .descriptor
            .binding_request_bounds
            .max_request_bytes = u64::try_from(encoded_len - 1).unwrap();
        assert_eq!(
            core.capture_pinned_selection(&instance).unwrap_err(),
            format!(
                "pinned selection is {encoded_len} bytes (max {})",
                encoded_len - 1
            )
        );
    }
}
