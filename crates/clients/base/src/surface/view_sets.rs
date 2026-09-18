//! Signed initial view-set composition. This is presentation data inside a
//! surface, not a second kind, view resolver or execution authority.

use super::{SlotsSpec, TilingSpec, ViewKindSpec};
use crate::ids::ViewGroupId;
use crate::layout::{
    LayoutTree, MAX_LAYOUT_DEPTH, MAX_LAYOUT_TILES, MAX_SPLIT_RATIO, MIN_SPLIT_RATIO, SplitAxis,
};
use crate::view_set::ViewSet;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MAX_VIEW_SETS: usize = 16;
pub const MAX_VIEW_SET_LABEL_BYTES: usize = 128;
pub const MAX_SAVED_VIEW_SETS: usize = 64;
pub const MAX_SAVED_VIEW_SET_ID_BYTES: usize = 64;
pub const MAX_SAVED_VIEW_SET_LIBRARY_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewSetSeedSpec {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub root: Option<LayoutSeedSpec>,
    #[serde(default)]
    pub slots: SlotsSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LayoutSeedSpec {
    Group {
        views: Vec<ViewKindSpec>,
        active: usize,
    },
    Split {
        axis: SplitAxis,
        ratio: f32,
        first: Box<Self>,
        second: Box<Self>,
    },
}

/// One reusable personal composition. This is deliberately only a template:
/// it carries no mounted identities, drafts, observations, compiled grants or
/// retained session authority. Opening it allocates a fresh view set and
/// revalidates every referenced view against the current compiled surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedViewSetTemplate {
    pub id: String,
    pub name: String,
    pub composition: ViewSetSeedSpec,
}

pub fn validate_saved_view_set_templates(templates: &[SavedViewSetTemplate]) -> Result<(), String> {
    let encoded = serde_json::to_vec(templates)
        .map_err(|error| format!("saved_view_sets cannot be encoded: {error}"))?;
    if encoded.len() > MAX_SAVED_VIEW_SET_LIBRARY_BYTES {
        return Err(format!(
            "saved_view_sets exceeds {MAX_SAVED_VIEW_SET_LIBRARY_BYTES} bytes"
        ));
    }
    if templates.len() > MAX_SAVED_VIEW_SETS {
        return Err(format!(
            "saved_view_sets exceeds {MAX_SAVED_VIEW_SETS} entries"
        ));
    }
    let mut ids = BTreeSet::new();
    for (index, template) in templates.iter().enumerate() {
        let location = format!("saved_view_sets[{index}]");
        if template.id.is_empty()
            || template.id.len() > MAX_SAVED_VIEW_SET_ID_BYTES
            || !template
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            || !ids.insert(&template.id)
        {
            return Err(format!("{location}.id must be a unique bounded identifier"));
        }
        if template.name.trim().is_empty()
            || template.name.len() > MAX_VIEW_SET_LABEL_BYTES
            || template.name.chars().any(char::is_control)
        {
            return Err(format!("{location}.name must be a nonempty bounded label"));
        }
        validate_seeds(std::slice::from_ref(&template.composition))
            .map_err(|error| format!("{location}.composition: {error}"))?;
    }
    Ok(())
}

pub fn validate_seeds(seeds: &[ViewSetSeedSpec]) -> Result<(), String> {
    if seeds.len() > MAX_VIEW_SETS {
        return Err(format!("view_sets exceeds {MAX_VIEW_SETS} entries"));
    }
    let mut ids = BTreeSet::new();
    let mut total_views = 0;
    for (index, seed) in seeds.iter().enumerate() {
        let location = format!("view_sets[{index}]");
        if seed.id.is_empty()
            || seed.id.len() > 64
            || !seed
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            || !ids.insert(&seed.id)
        {
            return Err(format!("{location}.id must be a unique bounded identifier"));
        }
        if seed.title.trim().is_empty()
            || seed.title.len() > MAX_VIEW_SET_LABEL_BYTES
            || seed.title.chars().any(char::is_control)
        {
            return Err(format!("{location}.title must be a nonempty bounded label"));
        }
        let mut pending = seed
            .root
            .as_ref()
            .map(|root| vec![(root, 1, format!("{location}.root"))])
            .unwrap_or_default();
        while let Some((node, depth, path)) = pending.pop() {
            if depth > MAX_LAYOUT_DEPTH {
                return Err(format!("{path} exceeds layout depth {MAX_LAYOUT_DEPTH}"));
            }
            match node {
                LayoutSeedSpec::Group { views, active } => {
                    if views.is_empty() || *active >= views.len() {
                        return Err(format!("{path}: group requires views and an active member"));
                    }
                    total_views += views.len();
                    if total_views > MAX_LAYOUT_TILES {
                        return Err(format!(
                            "{path}: surface exceeds {MAX_LAYOUT_TILES} mounted views"
                        ));
                    }
                }
                LayoutSeedSpec::Split {
                    ratio,
                    first,
                    second,
                    ..
                } => {
                    if !ratio.is_finite() || !(MIN_SPLIT_RATIO..=MAX_SPLIT_RATIO).contains(ratio) {
                        return Err(format!(
                            "{path}.ratio is outside supported split proportions"
                        ));
                    }
                    pending.push((second, depth + 1, format!("{path}.second")));
                    pending.push((first, depth + 1, format!("{path}.first")));
                }
            }
        }
    }
    Ok(())
}

impl ViewSetSeedSpec {
    pub fn instantiate(&self, tiling: &TilingSpec) -> Result<ViewSet, String> {
        validate_seeds(std::slice::from_ref(self))?;
        fn collect(node: &LayoutSeedSpec, views: &mut Vec<crate::view_set::ViewSpec>) {
            match node {
                LayoutSeedSpec::Group { views: group, .. } => {
                    views.extend(group.iter().map(ViewKindSpec::to_view_spec))
                }
                LayoutSeedSpec::Split { first, second, .. } => {
                    collect(first, views);
                    collect(second, views);
                }
            }
        }
        let mut views = Vec::new();
        if let Some(root) = &self.root {
            collect(root, &mut views);
        }
        // Allocate mounted identities once. Authored tree membership references
        // those instances; it never creates a second mutable view collection.
        let mut view_set = ViewSet::from_tiling(tiling.clone(), views);
        let mut ids: Vec<_> = view_set.tiles.keys().copied().collect();
        ids.sort_by_key(|id| id.0);
        fn instantiate(
            node: &LayoutSeedSpec,
            ids: &mut std::slice::Iter<'_, crate::ids::TileId>,
        ) -> LayoutTree {
            match node {
                LayoutSeedSpec::Group { views, active } => {
                    let tabs: Vec<_> = ids.by_ref().take(views.len()).copied().collect();
                    LayoutTree::Group {
                        group_id: ViewGroupId::new(tabs[0].0),
                        active: tabs[*active],
                        tabs,
                    }
                }
                LayoutSeedSpec::Split {
                    axis,
                    ratio,
                    first,
                    second,
                } => LayoutTree::Split {
                    axis: *axis,
                    ratio: *ratio,
                    first: Box::new(instantiate(first, ids)),
                    second: Box::new(instantiate(second, ids)),
                },
            }
        }
        view_set.root = self
            .root
            .as_ref()
            .map(|root| instantiate(root, &mut ids.iter()));
        view_set.title = self.title.clone();
        view_set.docks = crate::ui::model::RyeOsDockState::from_slots(&self.slots);
        if let Some(root) = &view_set.root {
            root.validate()?;
            let first = root.active_tile_ids()[0];
            view_set.focus_tile(first);
        }
        Ok(view_set)
    }
}

/// Shared daemon/client check. Nested references still resolve through the
/// ordinary surface view collector, including inactive group tabs.
pub fn validate_effective_view_sets(value: &serde_json::Value) -> Result<(), String> {
    let Some(raw) = value.get("view_sets") else {
        return Ok(());
    };
    let seeds: Vec<ViewSetSeedSpec> =
        serde_json::from_value(raw.clone()).map_err(|error| format!("view_sets: {error}"))?;
    validate_seeds(&seeds)?;
    if !seeds.is_empty()
        && (value
            .get("tiles")
            .and_then(|v| v.as_array())
            .is_some_and(|v| !v.is_empty())
            || value
                .get("slots")
                .and_then(|v| v.as_object())
                .is_some_and(|v| v.values().any(|slot| !slot.is_null())))
    {
        return Err("view_sets cannot be combined with surface-level tiles or slots; author each view set's composition explicitly".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn seed() -> serde_json::Value {
        json!({ "id": "work", "title": "Work", "root": {
            "type": "split", "axis": "horizontal", "ratio": 0.7,
            "first": { "type": "group", "views": ["view:test/conversation", "view:test/evidence"], "active": 1 },
            "second": { "type": "group", "views": ["view:test/changes"], "active": 0 }
        } })
    }

    #[test]
    fn authored_groups_preserve_selection_and_allocate_fresh_mounts() {
        let spec: ViewSetSeedSpec = serde_json::from_value(seed()).unwrap();
        let first = spec.instantiate(&TilingSpec::default()).unwrap();
        let second = spec.instantiate(&TilingSpec::default()).unwrap();
        assert_eq!(first.title, "Work");
        assert_eq!(
            first.tiles[&first.focused_tile].view.view_ref,
            "view:test/evidence"
        );
        assert_eq!(first.root.as_ref().unwrap().active_tile_ids().len(), 2);
        assert_eq!(first.tiles.len(), 3);
        assert!(
            first
                .tile_ids()
                .iter()
                .all(|id| !second.tiles.contains_key(id))
        );
    }

    #[test]
    fn malformed_compositions_fail_with_the_authored_location() {
        let mut bad = seed();
        bad["root"]["first"]["active"] = json!(9);
        let error = validate_effective_view_sets(&json!({ "view_sets": [bad] })).unwrap_err();
        assert!(error.contains("view_sets[0].root.first"));
        let mut bad = seed();
        bad["root"]["ratio"] = json!(1.0);
        assert!(
            validate_effective_view_sets(&json!({ "view_sets": [bad] }))
                .unwrap_err()
                .contains("root.ratio")
        );
        assert!(validate_effective_view_sets(&json!({ "view_sets": [seed(), seed()] })).is_err());
        assert!(
            validate_effective_view_sets(
                &json!({ "view_sets": [seed()], "tiles": ["view:test/other"] })
            )
            .is_err()
        );
        assert!(validate_effective_view_sets(&json!({ "view_sets": [seed()], "slots": { "left": { "content": "view:test/other" } } })).is_err());
    }

    #[test]
    fn core_mounts_only_the_named_view_sets_and_their_slots() {
        let mut work = seed();
        work["slots"] =
            json!({ "left": { "content": "view:test/tree", "open": true, "size": 24 } });
        let session = crate::ui::model::BrowserSession {
            effective_surface: Some(
                json!({ "name": "test", "view_sets": [work, { "id": "review", "title": "Review" }] }),
            ),
            ..Default::default()
        };
        let core = crate::ui::model::RyeOsCore::new(session, Default::default(), 0);
        assert_eq!(core.view_sets.len(), 2);
        assert!(core.view_sets[0].docks.left.is_some());
        assert!(core.view_sets[1].docks.left.is_none());
        assert_eq!(core.view_sets[1].title, "Review");
        assert!(core.view_sets[1].center_is_empty());
    }
}
