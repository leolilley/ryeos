//! Retained mount identity. Pure allocation; admission and authority remain daemon-owned.
use super::binding::UiBindingAttachmentCoordinate;
use crate::ids::{
    PresentationId, RyeOsViewInstanceKey, SurfaceInstanceId, TileId, ViewGroupId, ViewSetId,
};
use crate::layout::LayoutTree;
use crate::surface::view_sets::{LayoutSeedSpec, ViewSetSeedSpec, validate_seeds};
use crate::surface::{TilingSpec, ViewKindSpec};
use crate::view_set::{TileState, ViewSet};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSurfaceMountManifest {
    pub schema: u32,
    pub presentation_id: PresentationId,
    pub surface_instance_id: SurfaceInstanceId,
    pub allocation_start: u64,
    pub next_mount_id: u64,
    pub tabs: Vec<UiViewSetMountManifest>,
    pub active_view_set_id: Option<ViewSetId>,
}
#[cfg(test)]
mod tests {
    use super::*;
    fn manifest() -> UiSurfaceMountManifest {
        let seed:ViewSetSeedSpec=serde_json::from_value(serde_json::json!({"schema":2,"id":"development","title":"Development","root":{"type":"split","axis":"horizontal","ratio":0.5,"first":{"type":"group","views":["view:ryeos/a","view:ryeos/a"],"active":1},"second":{"type":"group","views":["view:ryeos/b"],"active":0}}})).unwrap();
        UiSurfaceMountManifest::allocate(
            PresentationId::try_new("presentation").unwrap(),
            SurfaceInstanceId::try_new("surface").unwrap(),
            1,
            vec![(
                seed,
                UiBindingAttachmentCoordinate {
                    binding_attachment_id: "binding".into(),
                    binding_generation: 1,
                    binding_digest: "digest".into(),
                },
            )],
            Some("development"),
        )
        .unwrap()
    }
    #[test]
    fn exact_mounts_do_not_depend_on_legacy_allocations() {
        let manifest = manifest();
        let before = manifest
            .instantiate_with_exact_ids(&TilingSpec::default())
            .unwrap();
        for _ in 0..7 {
            let _ = ViewSet::from_tiling(
                TilingSpec::default(),
                Vec::new(),
                &crate::ids::PresentationId::try_new("allocation-fixture").unwrap(),
                &crate::ids::SurfaceInstanceId::try_new("allocation-surface").unwrap(),
            );
        }
        let after = manifest
            .instantiate_with_exact_ids(&TilingSpec::default())
            .unwrap();
        assert_eq!(before[0].id, after[0].id);
        assert_eq!(before[0].root, after[0].root);
        assert_eq!(before[0].focused_tile, manifest.tabs[0].tile_ids[1]);
        assert_ne!(
            before[0].tiles[&manifest.tabs[0].tile_ids[0]].instance_key,
            before[0].tiles[&manifest.tabs[0].tile_ids[1]].instance_key
        );
    }
    #[test]
    fn malformed_allocation_is_rejected_without_repair() {
        let valid = manifest();
        let mut duplicate = valid.clone();
        duplicate.tabs[0].tile_ids[1] = duplicate.tabs[0].tile_ids[0];
        assert!(duplicate.validate().is_err());
        let mut gap = valid.clone();
        gap.tabs[0].group_ids[0].0 += 1;
        assert!(gap.validate().is_err());
        let mut frontier = valid.clone();
        frontier.next_mount_id += 1;
        assert!(frontier.validate().is_err());
        let mut schema = valid.clone();
        schema.schema = 0;
        assert!(schema.validate().is_err());
        let args = vec![(valid.tabs[0].seed.clone(), valid.tabs[0].binding.clone())];
        assert!(
            UiSurfaceMountManifest::allocate(
                valid.presentation_id,
                valid.surface_instance_id,
                u64::MAX,
                args,
                Some("development")
            )
            .is_err()
        );
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiViewSetMountManifest {
    pub view_set_id: ViewSetId,
    pub seed: ViewSetSeedSpec,
    pub tile_ids: Vec<TileId>,
    pub group_ids: Vec<ViewGroupId>,
    pub binding: UiBindingAttachmentCoordinate,
}
fn counts(node: &LayoutSeedSpec) -> (usize, usize) {
    match node {
        LayoutSeedSpec::Group { views, .. } => (views.len(), 1),
        LayoutSeedSpec::Split { first, second, .. } => {
            let a = counts(first);
            let b = counts(second);
            (a.0 + b.0, a.1 + b.1)
        }
    }
}
fn take(next: &mut u64) -> Result<u64, String> {
    let id = *next;
    *next = next
        .checked_add(1)
        .ok_or("mounted identity allocation exhausted")?;
    Ok(id)
}
impl UiSurfaceMountManifest {
    pub fn allocate(
        presentation_id: PresentationId,
        surface_instance_id: SurfaceInstanceId,
        allocation_start: u64,
        tabs: Vec<(ViewSetSeedSpec, UiBindingAttachmentCoordinate)>,
        active_seed_id: Option<&str>,
    ) -> Result<Self, String> {
        if allocation_start == 0 {
            return Err("mounted identity allocation must start above zero".into());
        }
        validate_seeds(
            &tabs
                .iter()
                .map(|(seed, _)| seed.clone())
                .collect::<Vec<_>>(),
        )?;
        let mut next = allocation_start;
        let mut mounted = Vec::new();
        let mut active = None;
        for (seed, binding) in tabs {
            let view_set_id = ViewSetId::new(take(&mut next)?);
            if active_seed_id == Some(seed.id.as_str()) {
                active = Some(view_set_id);
            }
            let (tiles, groups) = seed.root.as_ref().map(counts).unwrap_or((0, 0));
            let tile_ids = (0..tiles)
                .map(|_| take(&mut next).map(TileId::new))
                .collect::<Result<Vec<_>, _>>()?;
            let group_ids = (0..groups)
                .map(|_| take(&mut next).map(ViewGroupId::new))
                .collect::<Result<Vec<_>, _>>()?;
            mounted.push(UiViewSetMountManifest {
                view_set_id,
                seed,
                tile_ids,
                group_ids,
                binding,
            });
        }
        let result = Self {
            schema: 1,
            presentation_id,
            surface_instance_id,
            allocation_start,
            next_mount_id: next,
            tabs: mounted,
            active_view_set_id: active,
        };
        if active_seed_id.is_some() && active.is_none() {
            return Err("active seed is absent".into());
        }
        result.validate()?;
        Ok(result)
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != 1 || self.allocation_start == 0 {
            return Err("invalid mount manifest schema or allocation start".into());
        }
        validate_seeds(
            &self
                .tabs
                .iter()
                .map(|tab| tab.seed.clone())
                .collect::<Vec<_>>(),
        )?;
        if self.tabs.is_empty() != self.active_view_set_id.is_none()
            || self
                .active_view_set_id
                .is_some_and(|id| !self.tabs.iter().any(|tab| tab.view_set_id == id))
        {
            return Err("active tab contradicts manifest".into());
        }
        let mut next = self.allocation_start;
        for tab in &self.tabs {
            if tab.binding.binding_attachment_id.is_empty()
                || tab.binding.binding_generation == 0
                || tab.binding.binding_digest.is_empty()
            {
                return Err("empty exact mount binding".into());
            }
            if tab.view_set_id.0 != take(&mut next)? {
                return Err("noncanonical tab identity".into());
            }
            let (tiles, groups) = tab.seed.root.as_ref().map(counts).unwrap_or((0, 0));
            if tab.tile_ids.len() != tiles || tab.group_ids.len() != groups {
                return Err("manifest does not bijectively map seed slots".into());
            }
            for id in &tab.tile_ids {
                if id.0 != take(&mut next)? {
                    return Err("noncanonical tile identity".into());
                }
            }
            for id in &tab.group_ids {
                if id.0 != take(&mut next)? {
                    return Err("noncanonical group identity".into());
                }
            }
        }
        if next != self.next_mount_id {
            return Err("manifest allocation frontier differs".into());
        }
        Ok(())
    }
    pub fn instantiate_with_exact_ids(&self, tiling: &TilingSpec) -> Result<Vec<ViewSet>, String> {
        self.validate()?;
        let mut result = Vec::new();
        for tab in &self.tabs {
            let mut tiles = std::collections::HashMap::new();
            let mut ids = tab.tile_ids.iter();
            let mut groups = tab.group_ids.iter();
            fn build(
                node: &LayoutSeedSpec,
                ids: &mut std::slice::Iter<'_, TileId>,
                groups: &mut std::slice::Iter<'_, ViewGroupId>,
                tiles: &mut std::collections::HashMap<TileId, TileState>,
                presentation: &PresentationId,
                surface: &SurfaceInstanceId,
            ) -> LayoutTree {
                match node {
                    LayoutSeedSpec::Group {
                        label,
                        views,
                        active,
                    } => {
                        let group_id = *groups.next().expect("validated group count");
                        let tabs = views
                            .iter()
                            .map(|kind: &ViewKindSpec| {
                                let id = *ids.next().expect("validated tile count");
                                let view = kind.to_view_spec();
                                tiles.insert(
                                    id,
                                    TileState {
                                        instance_key: RyeOsViewInstanceKey::surface_tile(
                                            presentation,
                                            surface,
                                            id,
                                        ),
                                        local: view.initial_local_state(),
                                        view,
                                    },
                                );
                                id
                            })
                            .collect::<Vec<_>>();
                        LayoutTree::Group {
                            group_id,
                            label: label.clone(),
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
                        first: Box::new(build(first, ids, groups, tiles, presentation, surface)),
                        second: Box::new(build(second, ids, groups, tiles, presentation, surface)),
                    },
                }
            }
            let root = tab.seed.root.as_ref().map(|node| {
                build(
                    node,
                    &mut ids,
                    &mut groups,
                    &mut tiles,
                    &self.presentation_id,
                    &self.surface_instance_id,
                )
            });
            if let Some(root) = &root {
                root.validate()?;
            }
            let focused_tile = root
                .as_ref()
                .and_then(|root| root.active_tile_ids().first().copied())
                .unwrap_or(TileId::new(0));
            let mut set = ViewSet {
                id: tab.view_set_id,
                title: tab.seed.title.clone(),
                focus_target: None,
                tiling: tiling.clone(),
                root,
                tiles,
                focused_tile,
                maximized_tile: None,
            };
            if set.root.is_some() {
                set.focus_tile(focused_tile);
            }
            result.push(set);
        }
        Ok(result)
    }
}
