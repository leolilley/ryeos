//! Runtime subject attachment for mounted views.
//!
//! Attachments are presentation/session state keyed by mounted instance. They
//! are deliberately outside reusable layout preferences: a pinned observation
//! is not portable authority and opening a saved composition starts with fresh
//! follow-own-set relationships.

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
pub(crate) fn selection_dependencies(binding: &ViewBinding) -> BTreeSet<String> {
    facet_dependencies(binding)
        .into_iter()
        .filter(|path| is_selection_facet(path))
        .collect()
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

fn facet_candidates(path: &str) -> impl Iterator<Item = &str> {
    let mut dots = path
        .char_indices()
        .filter_map(|(index, character)| (character == '.').then_some(index))
        .collect::<Vec<_>>();
    dots.reverse();
    std::iter::once(path).chain(dots.into_iter().map(move |index| &path[..index]))
}

impl RyeOsCore {
    pub(crate) fn selection_attachment_for_instance(
        &self,
        instance: &RyeOsViewInstanceKey,
    ) -> Option<SelectionAttachment> {
        if let Some(attachment) = self.selection_attachments.get(instance) {
            return Some(attachment.clone());
        }
        let index = self.view_set_index_for_instance(instance)?;
        Some(SelectionAttachment::FollowViewSet {
            view_set_id: self.view_sets[index].id,
        })
    }

    pub(crate) fn followed_selection_view_set(
        &self,
        instance: &RyeOsViewInstanceKey,
    ) -> Option<ViewSetId> {
        match self.selection_attachments.get(instance) {
            Some(SelectionAttachment::FollowViewSet { view_set_id }) => self
                .view_sets
                .iter()
                .any(|view_set| view_set.id == *view_set_id)
                .then_some(*view_set_id),
            Some(SelectionAttachment::Pinned { .. }) => None,
            None => self
                .view_set_index_for_instance(instance)
                .map(|index| self.view_sets[index].id),
        }
    }

    /// Resolve one exact facet key. `resolve_params` owns dotted-field
    /// fallback and calls this repeatedly with progressively shorter keys.
    pub(crate) fn facet_value_for_instance(
        &self,
        instance: &RyeOsViewInstanceKey,
        logical_facet: &str,
    ) -> Option<Value> {
        let fold = self.seat.fold();
        if !is_selection_facet(logical_facet) {
            return fold.get(logical_facet).cloned();
        }
        match self.selection_attachments.get(instance) {
            Some(SelectionAttachment::Pinned { values, .. }) => values.get(logical_facet).cloned(),
            Some(SelectionAttachment::FollowViewSet { .. }) | None => {
                let view_set_id = self.followed_selection_view_set(instance)?;
                let key = super::seat::selection_storage_key(view_set_id, logical_facet)?;
                fold.get(&key).cloned()
            }
        }
    }

    /// Capture only selection roots actually read by the mounted binding. The
    /// values come from the current effective attachment, never from a client
    /// payload, and remain bounded runtime state.
    pub(crate) fn capture_pinned_selection(
        &self,
        instance: &RyeOsViewInstanceKey,
    ) -> Result<SelectionAttachment, String> {
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
            if let Some((key, value)) = facet_candidates(&dependency).find_map(|candidate| {
                self.facet_value_for_instance(instance, candidate)
                    .map(|value| (candidate.to_string(), value))
            }) {
                values.insert(key, value);
            }
        }
        let encoded = serde_json::to_vec(&values).map_err(|error| error.to_string())?;
        let byte_limit = self
            .binding_attachment_for_instance(instance)
            .ok_or("active session request bounds are unavailable")?
            .binding_request_bounds
            .max_request_bytes;
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
        let fingerprint = format!("{:x}", Sha256::digest(&encoded));
        Ok(SelectionAttachment::Pinned {
            values,
            fingerprint,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::reducer::test_support::*;

    fn mounted_selection_view() -> (RyeOsCore, RyeOsViewInstanceKey) {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/selection",
            serde_json::json!({
                "widget": "rows",
                "sources": {"default": {
                    "ref": "service:test/source",
                    "params": {"thread": "@facet:selection.work.thread"}
                }}
            }),
        );
        core.add_center_tile(ViewSpec::bound("view:test/selection"));
        let tile = core.view_sets[core.active_view_set].focused_tile;
        let instance = core.view_sets[core.active_view_set].tiles[&tile]
            .instance_key
            .clone();
        (core, instance)
    }

    #[test]
    fn absent_attachment_follows_the_containing_view_set() {
        let (mut core, instance) = mounted_selection_view();
        let owner = core.view_sets[core.active_view_set].id;
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
        let owner = core.view_sets[core.active_view_set].id;
        let key = super::super::seat::selection_storage_key(owner, "selection.work").unwrap();
        core.seat
            .append_facet(key.clone(), serde_json::json!({"thread": "T-one"}));
        let pinned = core.capture_pinned_selection(&instance).unwrap();
        core.selection_attachments
            .insert(instance.clone(), pinned.clone());
        core.seat
            .append_facet(key, serde_json::json!({"thread": "T-two"}));

        assert_eq!(
            core.facet_value_for_instance(&instance, "selection.work"),
            Some(serde_json::json!({"thread": "T-one"}))
        );
        assert!(matches!(pinned, SelectionAttachment::Pinned { .. }));
        assert!(
            core.facet_storage_key_for_instance(&instance, "selection.work")
                .is_none()
        );
        let preferences = core.export_layout_preferences().unwrap();
        assert!(!preferences.contains("selection_attachments"));
        assert!(!preferences.contains("T-one"));
    }

    #[test]
    fn pin_refuses_writer_only_binding() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/writer",
            serde_json::json!({
                "widget": "rows",
                "affordances": [{
                    "id": "select",
                    "invoke": {"plane": "ui", "facet": "selection", "value": {}}
                }]
            }),
        );
        core.add_center_tile(ViewSpec::bound("view:test/writer"));
        let tile = core.view_sets[core.active_view_set].focused_tile;
        let instance = core.view_sets[core.active_view_set].tiles[&tile]
            .instance_key
            .clone();

        assert_eq!(
            core.capture_pinned_selection(&instance).unwrap_err(),
            "view has no readable selection dependency to pin"
        );
        assert!(participates_in_selection(
            core.binding_for_insertion(core.view_sets[core.active_view_set].id, "view:test/writer")
                .unwrap()
        ));
    }

    #[test]
    fn unrelated_binding_has_no_selection_participation() {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        seed_view(&mut core, "view:test/unrelated");
        assert!(!participates_in_selection(
            core.binding_for_insertion(
                core.view_sets[core.active_view_set].id,
                "view:test/unrelated"
            )
            .unwrap()
        ));
    }

    #[test]
    fn pin_freezes_an_unresolved_declared_dependency_as_absent() {
        let (mut core, instance) = mounted_selection_view();
        let pinned = core.capture_pinned_selection(&instance).unwrap();
        core.selection_attachments.insert(instance.clone(), pinned);

        let owner = core.view_sets[core.active_view_set].id;
        let key = super::super::seat::selection_storage_key(owner, "selection.work").unwrap();
        core.seat
            .append_facet(key, serde_json::json!({"thread": "T-later"}));

        assert_eq!(
            core.facet_value_for_instance(&instance, "selection.work"),
            None
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
            "active session request bounds are unavailable"
        );
    }

    #[test]
    fn pin_accepts_the_exact_byte_limit_and_rejects_one_byte_over() {
        let (mut core, instance) = mounted_selection_view();
        let owner = core.view_sets[core.active_view_set].id;
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
