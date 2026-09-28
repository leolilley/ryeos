//! Layout system — split tree to positioned rectangles.
//!
//! Binary splits allocate space; leaf groups retain ordered view instances.
//! Group selection is shared state, never a renderer-local tab registry.

use crate::ids::{TileId, ViewGroupId};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Structural limits for the shared layout contract, not platform policy.
/// Keep restoration and editing on the same validator.
pub const MAX_LAYOUT_DEPTH: usize = 32;
pub const MAX_LAYOUT_TILES: usize = 256;
pub const MIN_SPLIT_RATIO: f32 = 0.1;
pub const MAX_SPLIT_RATIO: f32 = 0.9;

/// Positioned rectangle for a tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }

    pub fn zero() -> Self {
        Self {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        }
    }

    pub fn area(&self) -> u32 {
        self.w as u32 * self.h as u32
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }
}

/// Split axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    Horizontal, // left/right split
    Vertical,   // top/bottom split
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitBranch {
    First,
    Second,
}

/// Recursive layout tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LayoutTree {
    Group {
        group_id: ViewGroupId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        tabs: Vec<TileId>,
        active: TileId,
    },
    Split {
        axis: SplitAxis,
        ratio: f32,
        first: Box<LayoutTree>,
        second: Box<LayoutTree>,
    },
}

impl LayoutTree {
    /// A renderer path is usable only under its exact layout guard. Shared
    /// validation rejects stale paths and out-of-range sizes, never repairs them.
    pub fn set_split_ratio(&mut self, path: &[SplitBranch], ratio: f32) -> bool {
        if path.len() >= MAX_LAYOUT_DEPTH
            || !ratio.is_finite()
            || !(MIN_SPLIT_RATIO..=MAX_SPLIT_RATIO).contains(&ratio)
        {
            return false;
        }
        let mut node = self;
        for branch in path {
            let Self::Split { first, second, .. } = node else {
                return false;
            };
            node = match branch {
                SplitBranch::First => first,
                SplitBranch::Second => second,
            };
        }
        let Self::Split { ratio: current, .. } = node else {
            return false;
        };
        if *current == ratio {
            return false;
        }
        *current = ratio;
        true
    }

    pub fn single(tile: TileId) -> Self {
        Self::Group {
            group_id: ViewGroupId::new(tile.0),
            label: None,
            tabs: vec![tile],
            active: tile,
        }
    }

    /// Structural validation is shared by editing and restoration. Never
    /// repair malformed geometry or duplicate view membership in a renderer.
    pub fn validate(&self) -> Result<(), &'static str> {
        let mut pending = vec![(self, 1)];
        let mut instances = HashSet::new();
        let mut groups = HashSet::new();
        while let Some((node, depth)) = pending.pop() {
            if depth > MAX_LAYOUT_DEPTH {
                return Err("layout exceeds maximum depth");
            }
            match node {
                Self::Group {
                    group_id,
                    tabs,
                    active,
                    ..
                } => {
                    if tabs.is_empty() || !tabs.contains(active) {
                        return Err("layout group selection is invalid");
                    }
                    for id in tabs {
                        if !instances.insert(*id) {
                            return Err("layout contains duplicate tile identity");
                        }
                        if instances.len() > MAX_LAYOUT_TILES {
                            return Err("layout exceeds maximum tile count");
                        }
                    }
                    if !groups.insert(*group_id) {
                        return Err("layout contains duplicate group identity");
                    }
                }
                Self::Split {
                    ratio,
                    first,
                    second,
                    ..
                } => {
                    if !ratio.is_finite() || !(MIN_SPLIT_RATIO..=MAX_SPLIT_RATIO).contains(ratio) {
                        return Err("layout split ratio is invalid");
                    }
                    pending.push((first, depth + 1));
                    pending.push((second, depth + 1));
                }
            }
        }
        Ok(())
    }

    pub fn tile_ids(&self) -> Vec<TileId> {
        match self {
            Self::Group { tabs, .. } => tabs.clone(),
            Self::Split { first, second, .. } => {
                let mut ids = first.tile_ids();
                ids.extend(second.tile_ids());
                ids
            }
        }
    }

    pub fn active_tile_ids(&self) -> Vec<TileId> {
        match self {
            Self::Group { active, .. } => vec![*active],
            Self::Split { first, second, .. } => {
                let mut ids = first.active_tile_ids();
                ids.extend(second.active_tile_ids());
                ids
            }
        }
    }

    pub fn group_tabs(&self, target: TileId) -> Option<&[TileId]> {
        match self {
            Self::Group { tabs, .. } => tabs.contains(&target).then_some(tabs.as_slice()),
            Self::Split { first, second, .. } => first
                .group_tabs(target)
                .or_else(|| second.group_tabs(target)),
        }
    }

    fn max_group_id(&self) -> u64 {
        match self {
            Self::Group { group_id, .. } => group_id.0,
            Self::Split { first, second, .. } => first.max_group_id().max(second.max_group_id()),
        }
    }

    fn group_mut(&mut self, target: TileId) -> Option<&mut Self> {
        match self {
            Self::Group { tabs, .. } if tabs.contains(&target) => Some(self),
            Self::Group { .. } => None,
            Self::Split { first, second, .. } => {
                first.group_mut(target).or_else(|| second.group_mut(target))
            }
        }
    }

    pub fn select_tab(&mut self, tile: TileId) -> bool {
        let Some(Self::Group { active, .. }) = self.group_mut(tile) else {
            return false;
        };
        *active = tile;
        true
    }

    /// Split beside an exact group, preserving its tabs and all other splits.
    /// Failure leaves the original tree unchanged.
    pub fn split_tile(
        &mut self,
        target: TileId,
        incoming: TileId,
        axis: SplitAxis,
        before: bool,
        ratio: f32,
    ) -> bool {
        if self.validate().is_err() || self.tile_ids().contains(&incoming) {
            return false;
        }
        let Some(group_id) = self.max_group_id().checked_add(1).map(ViewGroupId::new) else {
            return false;
        };
        let mut next = self.clone();
        let Some(group) = next.group_mut(target) else {
            return false;
        };
        let incoming = Self::Group {
            group_id,
            label: None,
            tabs: vec![incoming],
            active: incoming,
        };
        let (first, second) = if before {
            (incoming, group.clone())
        } else {
            (group.clone(), incoming)
        };
        *group = Self::Split {
            axis,
            ratio,
            first: Box::new(first),
            second: Box::new(second),
        };
        if next.validate().is_err() {
            return false;
        }
        *self = next;
        true
    }

    /// Relocate a mounted view into a group, or reorder within that group.
    /// View state and authority are outside this tree and remain untouched.
    pub fn move_to_group(&mut self, tile: TileId, target: TileId, index: usize) -> bool {
        if tile == target || self.validate().is_err() || !self.tile_ids().contains(&tile) {
            return false;
        }
        let Some(mut next) = self.clone().without_tile(tile) else {
            return false;
        };
        let Some(Self::Group { tabs, active, .. }) = next.group_mut(target) else {
            return false;
        };
        if index > tabs.len() {
            return false;
        }
        tabs.insert(index, tile);
        *active = tile;
        if next.validate().is_err() {
            return false;
        }
        *self = next;
        true
    }

    /// Remove one placement. Empty groups collapse and their sibling survives.
    pub fn without_tile(self, target: TileId) -> Option<Self> {
        match self {
            Self::Group {
                group_id,
                label,
                mut tabs,
                mut active,
            } => {
                if let Some(index) = tabs.iter().position(|id| *id == target) {
                    tabs.remove(index);
                    if tabs.is_empty() {
                        return None;
                    }
                    if active == target {
                        active = tabs[index.min(tabs.len() - 1)];
                    }
                }
                Some(Self::Group {
                    group_id,
                    label,
                    tabs,
                    active,
                })
            }
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => match (first.without_tile(target), second.without_tile(target)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    axis,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (remaining, None) | (None, remaining) => remaining,
            },
        }
    }

    /// Reorder placements without rebuilding geometry. Group active positions
    /// follow the reordered membership; callers then focus the moved instance.
    pub fn reorder(&mut self, order: &[TileId]) -> bool {
        let current = self.tile_ids();
        if self.validate().is_err()
            || current.len() != order.len()
            || current.iter().copied().collect::<HashSet<_>>() != order.iter().copied().collect()
            || order.iter().copied().collect::<HashSet<_>>().len() != order.len()
        {
            return false;
        }
        fn assign(node: &mut LayoutTree, order: &mut impl Iterator<Item = TileId>) {
            match node {
                LayoutTree::Group { tabs, active, .. } => {
                    let selected = tabs
                        .iter()
                        .position(|id| id == active)
                        .expect("validated selected member");
                    for id in tabs.iter_mut() {
                        *id = order.next().expect("validated equal membership");
                    }
                    *active = tabs[selected];
                }
                LayoutTree::Split { first, second, .. } => {
                    assign(first, order);
                    assign(second, order);
                }
            }
        }
        assign(self, &mut order.iter().copied());
        true
    }

    /// Resize the nearest ancestor of a view on the requested axis.
    pub fn resize_nearest(&mut self, target: TileId, axis: SplitAxis, delta: f32) -> bool {
        fn visit(
            node: &mut LayoutTree,
            target: TileId,
            axis: SplitAxis,
            delta: f32,
        ) -> (bool, bool) {
            match node {
                LayoutTree::Group { tabs, .. } => (tabs.contains(&target), false),
                LayoutTree::Split {
                    axis: own_axis,
                    ratio,
                    first,
                    second,
                } => {
                    let (found, handled) = visit(first, target, axis, delta);
                    let (found, handled) = if found {
                        (found, handled)
                    } else {
                        visit(second, target, axis, delta)
                    };
                    if found && !handled && *own_axis == axis {
                        *ratio = (*ratio + delta).clamp(MIN_SPLIT_RATIO, MAX_SPLIT_RATIO);
                        return (true, true);
                    }
                    (found, handled)
                }
            }
        }
        if !delta.is_finite() || self.validate().is_err() {
            return false;
        }
        let previous = self.clone();
        visit(self, target, axis, delta);
        *self != previous
    }
}

/// Compute positioned rectangles for each tile in the tree.
pub fn layout_rects(tree: &LayoutTree, viewport: Rect) -> HashMap<TileId, Rect> {
    let mut rects = HashMap::new();
    layout_rects_recursive(tree, viewport, &mut rects);
    rects
}

fn layout_rects_recursive(tree: &LayoutTree, rect: Rect, out: &mut HashMap<TileId, Rect>) {
    match tree {
        LayoutTree::Group { active, .. } => {
            out.insert(*active, rect);
        }
        LayoutTree::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let ratio = ratio.clamp(0.1, 0.9);
            match axis {
                SplitAxis::Horizontal => {
                    let split_x = (rect.w as f32 * ratio) as u16;
                    let first_rect = Rect::new(rect.x, rect.y, split_x, rect.h);
                    let second_rect = Rect::new(
                        rect.x + split_x,
                        rect.y,
                        rect.w.saturating_sub(split_x),
                        rect.h,
                    );
                    layout_rects_recursive(first, first_rect, out);
                    layout_rects_recursive(second, second_rect, out);
                }
                SplitAxis::Vertical => {
                    let split_y = (rect.h as f32 * ratio) as u16;
                    let first_rect = Rect::new(rect.x, rect.y, rect.w, split_y);
                    let second_rect = Rect::new(
                        rect.x,
                        rect.y + split_y,
                        rect.w,
                        rect.h.saturating_sub(split_y),
                    );
                    layout_rects_recursive(first, first_rect, out);
                    layout_rects_recursive(second, second_rect, out);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn three_pane() -> LayoutTree {
        LayoutTree::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.25,
            first: Box::new(LayoutTree::single(TileId::new(1))),
            second: Box::new(LayoutTree::Split {
                axis: SplitAxis::Vertical,
                ratio: 0.85,
                first: Box::new(LayoutTree::single(TileId::new(2))),
                second: Box::new(LayoutTree::single(TileId::new(3))),
            }),
        }
    }

    #[test]
    fn layout_tree_collects_tile_ids() {
        let ids = three_pane().tile_ids();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&TileId::new(1)));
        assert!(ids.contains(&TileId::new(2)));
        assert!(ids.contains(&TileId::new(3)));
    }

    #[test]
    fn layout_split_rects_sum_to_viewport() {
        let tree = three_pane();
        let vp = Rect::new(0, 0, 200, 60);
        let rects = layout_rects(&tree, vp);

        assert_eq!(rects.len(), 3);

        // All rects should fit within viewport
        for rect in rects.values() {
            assert!(rect.x + rect.w <= vp.w);
            assert!(rect.y + rect.h <= vp.h);
        }

        // Total area should be close to viewport area
        let total: u32 = rects.values().map(|r| r.area()).sum();
        assert_eq!(total, vp.area());
    }

    #[test]
    fn layout_handles_tiny_viewport() {
        let tree = LayoutTree::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutTree::single(TileId::new(1))),
            second: Box::new(LayoutTree::single(TileId::new(2))),
        };
        let vp = Rect::new(0, 0, 10, 1);
        let rects = layout_rects(&tree, vp);
        assert_eq!(rects.len(), 2);
    }

    #[test]
    fn layout_handles_zero_viewport() {
        let tree = LayoutTree::single(TileId::new(1));
        let vp = Rect::new(0, 0, 0, 0);
        let rects = layout_rects(&tree, vp);
        assert_eq!(rects.len(), 1);
        assert!(rects[&TileId::new(1)].is_empty());
    }

    #[test]
    fn nested_split_preserves_unrelated_geometry_and_removal_collapses_only_parent() {
        let mut tree = three_pane();
        let original = tree.clone();
        assert!(tree.split_tile(
            TileId::new(2),
            TileId::new(4),
            SplitAxis::Horizontal,
            true,
            0.3
        ));
        tree.validate().unwrap();
        assert_eq!(
            tree.tile_ids(),
            vec![
                TileId::new(1),
                TileId::new(4),
                TileId::new(2),
                TileId::new(3)
            ]
        );
        assert_eq!(tree.without_tile(TileId::new(4)), Some(original));
    }

    #[test]
    fn refused_edits_do_not_mutate_layout() {
        let mut tree = three_pane();
        let original = tree.clone();
        for (target, incoming, ratio) in [(1, 2, 0.5), (99, 4, 0.5), (1, 4, f32::NAN), (1, 4, 0.0)]
        {
            assert!(!tree.split_tile(
                TileId::new(target),
                TileId::new(incoming),
                SplitAxis::Horizontal,
                false,
                ratio
            ));
            assert_eq!(tree, original);
        }
        assert!(!tree.reorder(&[TileId::new(1), TileId::new(1), TileId::new(3)]));
        assert_eq!(tree, original);
        assert!(!tree.resize_nearest(TileId::new(2), SplitAxis::Vertical, f32::INFINITY));
        assert_eq!(tree, original);
    }

    #[test]
    fn nested_resize_changes_nearest_matching_ancestor() {
        let mut tree = three_pane();
        assert!(tree.resize_nearest(TileId::new(3), SplitAxis::Vertical, -0.1));
        let LayoutTree::Split { ratio, second, .. } = &tree else {
            panic!("root split");
        };
        assert_eq!(*ratio, 0.25);
        let LayoutTree::Split { ratio, .. } = second.as_ref() else {
            panic!("nested split");
        };
        assert!((*ratio - 0.75).abs() < 0.00001);
    }

    #[test]
    fn validator_rejects_duplicate_placement_and_excessive_depth() {
        let duplicate = LayoutTree::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutTree::single(TileId::new(1))),
            second: Box::new(LayoutTree::single(TileId::new(1))),
        };
        assert_eq!(
            duplicate.validate(),
            Err("layout contains duplicate tile identity")
        );
        let mut tree = LayoutTree::single(TileId::new(0));
        for n in 1..=MAX_LAYOUT_DEPTH {
            tree = LayoutTree::Split {
                axis: SplitAxis::Vertical,
                ratio: 0.5,
                first: Box::new(tree),
                second: Box::new(LayoutTree::single(TileId::new(n as u64))),
            };
        }
        assert_eq!(tree.validate(), Err("layout exceeds maximum depth"));
    }

    #[test]
    fn grouping_preserves_membership_and_projects_only_active_view() {
        let mut tree = three_pane();
        assert!(tree.move_to_group(TileId::new(3), TileId::new(2), 1));
        tree.validate().unwrap();
        assert_eq!(
            tree.group_tabs(TileId::new(2)).unwrap(),
            &[TileId::new(2), TileId::new(3)]
        );
        assert_eq!(tree.active_tile_ids(), vec![TileId::new(1), TileId::new(3)]);
        assert!(tree.select_tab(TileId::new(2)));
        assert_eq!(layout_rects(&tree, Rect::new(0, 0, 120, 40)).len(), 2);
        assert_eq!(tree.tile_ids().len(), 3);
        let before = tree.clone();
        assert!(!tree.move_to_group(TileId::new(3), TileId::new(2), 99));
        assert_eq!(tree, before);
        let tree = tree.without_tile(TileId::new(2)).unwrap();
        tree.validate().unwrap();
        assert_eq!(tree.active_tile_ids(), vec![TileId::new(1), TileId::new(3)]);
    }

    #[test]
    fn split_next_to_group_preserves_all_its_tabs_and_selected_view() {
        let mut tree = three_pane();
        assert!(tree.move_to_group(TileId::new(3), TileId::new(2), 0));
        assert!(tree.split_tile(
            TileId::new(2),
            TileId::new(4),
            SplitAxis::Vertical,
            true,
            0.5
        ));
        assert_eq!(
            tree.group_tabs(TileId::new(2)).unwrap(),
            &[TileId::new(3), TileId::new(2)]
        );
        assert!(tree.active_tile_ids().contains(&TileId::new(3)));
        tree.validate().unwrap();
    }
}
