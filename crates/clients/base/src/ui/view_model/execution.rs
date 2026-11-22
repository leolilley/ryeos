use super::{RyeOsCore, RyeOsViewVm, view_vm_for_instance};

// Keep the established `ui::view_model::RyeOsTimelineEntryVm` path stable
// while grouping execution presentation behind this module.
pub use super::super::timeline::RyeOsTimelineEntryVm;

/// The response a facet-backed view renders: the seat-fold value at `facet`,
/// resolved through the shared `@facet:` grammar (so a dotted path like
/// `selection.summary` reads the field within the `selection` facet). `None`
/// when the facet is unset — the view then falls back to its `source` fetch.
pub(super) fn facet_backed_response(
    core: &RyeOsCore,
    instance: &crate::ids::RyeOsViewInstanceKey,
    facet: &str,
) -> Option<serde_json::Value> {
    let resolved = super::super::content::resolve_params(
        &serde_json::Value::String(format!("@facet:{facet}")),
        |key| core.facet_value_for_instance(instance, key),
    );
    (!resolved.is_null()).then_some(resolved)
}

/// The timeline entry under the point in the focused feed lens, if the focused
/// view is a timeline with a point on an entry. The single home for reading the
/// focused feed entry; the Enter intent derives from it.
pub(super) fn focused_timeline_entry(
    core: &RyeOsCore,
    instance: &crate::ids::RyeOsViewInstanceKey,
) -> Option<RyeOsTimelineEntryVm> {
    if let RyeOsViewVm::Timeline {
        entries, selected, ..
    } = view_vm_for_instance(core, instance)?
    {
        return selected.and_then(|i| entries.into_iter().nth(i));
    }
    None
}

/// The focused feed entry's secondary affordance, such as the retry a failed
/// outcome carries. Surfaced through the commands overlay (its Shift+Enter
/// secondary and a distinct "Retry failed turn" item), never a direct feed key,
/// so Enter stays inspect.
pub(super) fn retry_intent_for_focused_row(
    core: &RyeOsCore,
    instance: &crate::ids::RyeOsViewInstanceKey,
) -> Option<super::super::event::RyeOsUiIntent> {
    match focused_timeline_entry(core, instance)? {
        RyeOsTimelineEntryVm::Line {
            secondary_intent, ..
        } => secondary_intent,
        _ => None,
    }
}

#[cfg(test)]
mod facet_scope_tests {
    use super::*;
    use crate::ui::reducer::test_support::{core as fixture_core, session_with_surface};
    use serde_json::json;

    #[test]
    fn facet_backed_views_resolve_their_mount_not_active_or_global_selection() {
        let mut core = fixture_core(session_with_surface(json!({
            "name":"Facet scopes",
            "views":{"view:test/detail":{"widget":"rows"}},
            "view_sets":[
                {"schema":2,"id":"first","title":"First","root":{"type":"group","views":["view:test/detail"],"active":0}},
                {"schema":2,"id":"second","title":"Second","root":{"type":"group","views":["view:test/detail"],"active":0}}
            ]
        })));
        let mut instances = Vec::new();
        let sets = core
            .active_surface()
            .unwrap()
            .view_sets()
            .iter()
            .map(|set| (set.id, set.tiles[&set.focused_tile].instance_key.clone()))
            .collect::<Vec<_>>();
        for ((id, instance), value) in sets.into_iter().zip(["first", "second"]) {
            instances.push(instance);
            core.seat.append_facet(
                crate::ui::seat::selection_facet_key(id),
                json!({"summary": value}),
            );
        }
        core.seat
            .append_facet("selection", json!({"summary": "wrong-global"}));
        let first = &instances[0];
        let second = &instances[1];
        assert_eq!(
            facet_backed_response(&core, first, "selection.summary"),
            Some(json!("first"))
        );
        assert_eq!(
            facet_backed_response(&core, second, "selection.summary"),
            Some(json!("second"))
        );
    }
}
