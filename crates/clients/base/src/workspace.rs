//! Workspace — ordered center tiles + tiling algorithm, tile state, focus.
//!
//! The workspace never stores a layout tree. It holds an ordered tile
//! list and the surface-declared `TilingSpec`; `compute_layout` derives
//! the `LayoutTree` renderers consume. Zero tiles means home: the
//! center renders nothing and the ambient background owns the frame.

use crate::ids::TileId;
use crate::layout::{layout_rects, LayoutTree, Rect, SplitAxis};
use crate::surface::{ArrangeSpec, InsertSpec, SideSpec, TilingModeSpec, TilingSpec};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// View specs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ViewSpec {
    /// A `view:` item-bound tile (views-as-content). Every product
    /// concept renders through this — tiles are bindings, not code.
    Bound { view_ref: String },
    /// Engine ambient: 3D topology.
    Graph { graph_id: Option<String> },
    /// Engine ambient: 2D namespace atlas.
    Atlas,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusDirection {
    Left,
    Right,
    Up,
    Down,
}

// ---------------------------------------------------------------------------
// View-local state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum ViewLocalState {
    GenericList {
        cursor: usize,
        scroll: usize,
    },
    #[default]
    None,
}

// ---------------------------------------------------------------------------
// Input capability
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputCapability {
    Prompt,
    Filter,
    Navigate,
    None,
}

impl ViewSpec {
    pub fn initial_local_state(&self) -> ViewLocalState {
        match self {
            ViewSpec::Bound { .. } => ViewLocalState::GenericList {
                cursor: 0,
                scroll: 0,
            },
            ViewSpec::Atlas | ViewSpec::Graph { .. } => ViewLocalState::None,
        }
    }

    pub fn input_capability(&self) -> InputCapability {
        match self {
            ViewSpec::Bound { .. } | ViewSpec::Atlas | ViewSpec::Graph { .. } => {
                InputCapability::None
            }
        }
    }

    /// Human-readable label for the input context hint.
    pub fn input_hint(&self) -> &'static str {
        match self {
            ViewSpec::Bound { .. } => "view",
            ViewSpec::Atlas => "atlas",
            ViewSpec::Graph { .. } => "graph",
        }
    }

    /// Short title for tile header.
    pub fn title(&self) -> String {
        match self {
            ViewSpec::Bound { view_ref } => {
                view_ref.rsplit('/').next().unwrap_or(view_ref).to_string()
            }
            ViewSpec::Atlas => "Atlas".into(),
            ViewSpec::Graph { graph_id } => {
                if let Some(id) = graph_id {
                    format!("Graph: {}", id)
                } else {
                    "Graph".into()
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tile state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileState {
    pub view: ViewSpec,
    pub local: ViewLocalState,
}

// ---------------------------------------------------------------------------
// Layout computation — the tiling algorithm
// ---------------------------------------------------------------------------

/// Compute the layout tree for an ordered tile list under a tiling spec.
///
/// - 0 tiles → no tree (the center renders nothing).
/// - 1 tile → monocle: the tile takes the full center rect.
/// - n > 1 → master region on `master.side` at `master.ratio`; the first
///   `master.count` tiles arranged along `master.arrange`; the rest in
///   the stack region arranged along `stack.arrange`.
pub fn compute_layout(tiling: &TilingSpec, tiles: &[TileId]) -> Option<LayoutTree> {
    match tiling.mode {
        TilingModeSpec::MasterStack => master_stack_layout(tiling, tiles),
    }
}

fn master_stack_layout(tiling: &TilingSpec, tiles: &[TileId]) -> Option<LayoutTree> {
    match tiles {
        [] => None,
        [only] => Some(LayoutTree::Leaf(*only)),
        _ => {
            let count = tiling.master.count.clamp(1, tiles.len());
            let (masters, stack) = tiles.split_at(count);
            let master_tree = arrange_region(masters, tiling.master.arrange)?;
            let Some(stack_tree) = arrange_region(stack, tiling.stack.arrange) else {
                return Some(master_tree);
            };
            let ratio = tiling.master.ratio.clamp(0.1, 0.9);
            Some(match tiling.master.side {
                SideSpec::Left => LayoutTree::Split {
                    axis: SplitAxis::Horizontal,
                    ratio,
                    first: Box::new(master_tree),
                    second: Box::new(stack_tree),
                },
                SideSpec::Right => LayoutTree::Split {
                    axis: SplitAxis::Horizontal,
                    ratio: 1.0 - ratio,
                    first: Box::new(stack_tree),
                    second: Box::new(master_tree),
                },
            })
        }
    }
}

/// Even split of a region along one arrangement axis.
fn arrange_region(ids: &[TileId], arrange: ArrangeSpec) -> Option<LayoutTree> {
    let axis = match arrange {
        // Vertical arrangement stacks top-to-bottom → vertical splits.
        ArrangeSpec::Vertical => SplitAxis::Vertical,
        // Horizontal arrangement runs left-to-right → horizontal splits.
        ArrangeSpec::Horizontal => SplitAxis::Horizontal,
    };
    match ids {
        [] => None,
        [only] => Some(LayoutTree::Leaf(*only)),
        [first, rest @ ..] => Some(LayoutTree::Split {
            axis,
            ratio: 1.0 / ids.len() as f32,
            first: Box::new(LayoutTree::Leaf(*first)),
            second: Box::new(arrange_region(rest, arrange)?),
        }),
    }
}

// ---------------------------------------------------------------------------
// Workspace
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    /// The tiling algorithm (from the surface).
    pub tiling: TilingSpec,
    /// Ordered center tiles — the single ordering authority. The layout
    /// tree is computed from this list, never stored.
    pub center_tiles: Vec<TileId>,
    /// Per-tile view + local state.
    pub tiles: HashMap<TileId, TileState>,
    /// Focused tile. Dangling when the center is empty (home).
    pub focused_tile: TileId,
}

impl Workspace {
    /// Build a workspace from a tiling spec and ordered initial views.
    pub fn from_tiling(tiling: TilingSpec, views: Vec<ViewSpec>) -> Self {
        let mut center_tiles = Vec::with_capacity(views.len());
        let mut tiles = HashMap::new();
        for (index, view) in views.into_iter().enumerate() {
            let id = TileId::new(index as u64 + 1);
            center_tiles.push(id);
            tiles.insert(
                id,
                TileState {
                    local: view.initial_local_state(),
                    view,
                },
            );
        }
        let focused_tile = center_tiles
            .first()
            .copied()
            .unwrap_or_else(|| TileId::new(0));
        Self {
            tiling,
            center_tiles,
            tiles,
            focused_tile,
        }
    }

    /// The computed layout tree. None when the center is empty.
    pub fn layout(&self) -> Option<LayoutTree> {
        compute_layout(&self.tiling, &self.center_tiles)
    }

    /// Ordered center tile ids.
    pub fn tile_ids(&self) -> Vec<TileId> {
        self.center_tiles.clone()
    }

    /// Home: an empty center. The ambient background owns the frame.
    pub fn is_home(&self) -> bool {
        self.center_tiles.is_empty()
    }

    /// Clear the center back to home.
    pub fn reset_to_home(&mut self) {
        self.center_tiles.clear();
        self.tiles.clear();
        self.focused_tile = TileId::new(0);
    }

    /// Get the focused tile's view spec.
    pub fn focused_view(&self) -> Option<&ViewSpec> {
        self.tiles.get(&self.focused_tile).map(|t| &t.view)
    }

    /// Get the focused tile's view spec (mutable).
    pub fn focused_view_mut(&mut self) -> Option<&mut ViewSpec> {
        self.tiles.get_mut(&self.focused_tile).map(|t| &mut t.view)
    }

    /// Get input capability of focused tile.
    pub fn focused_capability(&self) -> InputCapability {
        self.focused_view()
            .map(|v| v.input_capability())
            .unwrap_or(InputCapability::None)
    }

    pub fn replace_focused_view(&mut self, view: ViewSpec) -> Option<TileId> {
        let tile = self.tiles.get_mut(&self.focused_tile)?;
        tile.local = view.initial_local_state();
        tile.view = view;
        Some(self.focused_tile)
    }

    /// Allocate a fresh TileId.
    fn next_tile_id() -> TileId {
        static COUNTER: AtomicU64 = AtomicU64::new(100);
        TileId::new(COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    /// Add a center tile per the tiling's insert policy (`end` appends:
    /// the new tile lands at the bottom of the stack region), then
    /// focuses it. The layout recomputes implicitly.
    pub fn add_tile(&mut self, view: ViewSpec) -> TileId {
        let id = Self::next_tile_id();
        match self.tiling.insert {
            InsertSpec::End => self.center_tiles.push(id),
        }
        self.tiles.insert(
            id,
            TileState {
                local: view.initial_local_state(),
                view,
            },
        );
        self.focused_tile = id;
        id
    }

    /// Close a tile by id, keeping the remaining order. Returns false if
    /// the tile is not in the center. Closing the last tile empties the
    /// center (home).
    pub fn close_tile(&mut self, tile_id: TileId) -> bool {
        let Some(position) = self.center_tiles.iter().position(|id| *id == tile_id) else {
            return false;
        };
        self.center_tiles.remove(position);
        self.tiles.remove(&tile_id);
        if self.focused_tile == tile_id {
            self.focused_tile = self
                .center_tiles
                .get(position.min(self.center_tiles.len().saturating_sub(1)))
                .copied()
                .unwrap_or_else(|| TileId::new(0));
        }
        true
    }

    /// Close the focused tile.
    pub fn close_focused(&mut self) -> bool {
        self.close_tile(self.focused_tile)
    }

    /// Focus next tile in center order.
    pub fn focus_next(&mut self) {
        let ids = &self.center_tiles;
        if let Some(pos) = ids.iter().position(|id| *id == self.focused_tile) {
            let next = (pos + 1) % ids.len();
            self.focused_tile = ids[next];
        }
    }

    /// Focus previous tile in center order.
    pub fn focus_prev(&mut self) {
        let ids = &self.center_tiles;
        if let Some(pos) = ids.iter().position(|id| *id == self.focused_tile) {
            let prev = if pos == 0 { ids.len() - 1 } else { pos - 1 };
            self.focused_tile = ids[prev];
        }
    }

    pub fn focus_in_direction(&mut self, direction: FocusDirection) -> bool {
        let Some(layout) = self.layout() else {
            return false;
        };
        let rects = layout_rects(&layout, Rect::new(0, 0, 10_000, 10_000));
        let Some(focused) = rects.get(&self.focused_tile).copied() else {
            return false;
        };
        let focused_center = rect_center(focused);
        let best = rects
            .iter()
            .filter(|(id, _)| **id != self.focused_tile)
            .filter_map(|(id, rect)| {
                let center = rect_center(*rect);
                let primary = match direction {
                    FocusDirection::Left => focused_center.0 - center.0,
                    FocusDirection::Right => center.0 - focused_center.0,
                    FocusDirection::Up => focused_center.1 - center.1,
                    FocusDirection::Down => center.1 - focused_center.1,
                };
                if primary <= 0 {
                    return None;
                }
                let perpendicular = match direction {
                    FocusDirection::Left | FocusDirection::Right => {
                        perpendicular_gap(focused.y, focused.h, rect.y, rect.h)
                    }
                    FocusDirection::Up | FocusDirection::Down => {
                        perpendicular_gap(focused.x, focused.w, rect.x, rect.w)
                    }
                };
                Some((*id, (perpendicular, primary)))
            })
            .min_by_key(|(_, score)| *score)
            .map(|(id, _)| id);
        let Some(tile_id) = best else {
            return false;
        };
        self.focused_tile = tile_id;
        true
    }

    /// Move a tile within the ordered list (wrapping). Order is the
    /// single authority: position decides master/stack membership.
    pub fn move_tile_in_stack(&mut self, tile_id: TileId, delta: i32) -> bool {
        let len = self.center_tiles.len();
        if len <= 1 {
            return false;
        }
        let Some(index) = self.center_tiles.iter().position(|id| *id == tile_id) else {
            return false;
        };
        let new_index = wrap_index(index, delta, len);
        if new_index == index {
            return false;
        }
        let moved = self.center_tiles.remove(index);
        self.center_tiles.insert(new_index, moved);
        self.focused_tile = tile_id;
        true
    }

    pub fn move_focused_in_stack(&mut self, delta: i32) -> bool {
        self.move_tile_in_stack(self.focused_tile, delta)
    }

    /// Zoom: promote a tile to the front of the order (into the master
    /// region). If it already leads, swap it with the next tile.
    pub fn zoom_tile(&mut self, tile_id: TileId) -> bool {
        let len = self.center_tiles.len();
        if len <= 1 {
            return false;
        }
        let Some(index) = self.center_tiles.iter().position(|id| *id == tile_id) else {
            return false;
        };
        if index == 0 {
            self.center_tiles.swap(0, 1);
        } else {
            let moved = self.center_tiles.remove(index);
            self.center_tiles.insert(0, moved);
        }
        self.focused_tile = tile_id;
        true
    }

    pub fn zoom_focused(&mut self) -> bool {
        self.zoom_tile(self.focused_tile)
    }

    /// Resize the master/stack boundary: left/right move the boundary in
    /// screen space regardless of which side the master sits on.
    pub fn resize_master(&mut self, direction: FocusDirection) -> bool {
        if self.center_tiles.len() <= 1 {
            return false;
        }
        let toward_master_growth = match (direction, self.tiling.master.side) {
            (FocusDirection::Left, SideSpec::Left) => -0.04,
            (FocusDirection::Left, SideSpec::Right) => 0.04,
            (FocusDirection::Right, SideSpec::Left) => 0.04,
            (FocusDirection::Right, SideSpec::Right) => -0.04,
            (FocusDirection::Up | FocusDirection::Down, _) => return false,
        };
        let next = (self.tiling.master.ratio + toward_master_growth).clamp(0.1, 0.9);
        if (next - self.tiling.master.ratio).abs() < f32::EPSILON {
            return false;
        }
        self.tiling.master.ratio = next;
        true
    }

    /// Move cursor up in the focused list view.
    pub fn cursor_up(&mut self) {
        if let Some(tile) = self.tiles.get_mut(&self.focused_tile) {
            match &mut tile.local {
                ViewLocalState::GenericList { cursor, .. } if *cursor > 0 => {
                    *cursor -= 1;
                }
                _ => {}
            }
        }
    }

    /// Move cursor down in the focused list view.
    pub fn cursor_down(&mut self, total_items: usize) {
        if let Some(tile) = self.tiles.get_mut(&self.focused_tile) {
            if total_items == 0 {
                return;
            }
            match &mut tile.local {
                ViewLocalState::GenericList { cursor, .. }
                    if *cursor < total_items.saturating_sub(1) =>
                {
                    *cursor += 1;
                }
                _ => {}
            }
        }
    }
}

fn wrap_index(index: usize, delta: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let len = len as i32;
    (index as i32 + delta).rem_euclid(len) as usize
}

fn rect_center(rect: Rect) -> (i32, i32) {
    (
        rect.x as i32 + rect.w as i32 / 2,
        rect.y as i32 + rect.h as i32 / 2,
    )
}

fn perpendicular_gap(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> i32 {
    let a_end = a_start as i32 + a_len as i32;
    let b_end = b_start as i32 + b_len as i32;
    if a_end < b_start as i32 {
        b_start as i32 - a_end
    } else if b_end < a_start as i32 {
        a_start as i32 - b_end
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::MasterSpec;

    fn ids(raw: &[u64]) -> Vec<TileId> {
        raw.iter().map(|n| TileId::new(*n)).collect()
    }

    fn bound(name: &str) -> ViewSpec {
        ViewSpec::Bound {
            view_ref: format!("view:test/{name}"),
        }
    }

    fn workspace_with(n: usize) -> Workspace {
        Workspace::from_tiling(
            TilingSpec::default(),
            (0..n).map(|i| bound(&format!("v{i}"))).collect(),
        )
    }

    #[test]
    fn compute_layout_empty_center_has_no_tree() {
        assert_eq!(compute_layout(&TilingSpec::default(), &[]), None);
    }

    #[test]
    fn compute_layout_single_tile_is_monocle() {
        let tree = compute_layout(&TilingSpec::default(), &ids(&[7])).unwrap();
        assert_eq!(tree, LayoutTree::Leaf(TileId::new(7)));
        let rects = layout_rects(&tree, Rect::new(0, 0, 120, 40));
        assert_eq!(rects[&TileId::new(7)], Rect::new(0, 0, 120, 40));
    }

    #[test]
    fn compute_layout_three_tiles_master_right_stack_horizontal() {
        // Default: master right at 0.6, count 1, stack horizontal.
        let tree = compute_layout(&TilingSpec::default(), &ids(&[1, 2, 3])).unwrap();
        let LayoutTree::Split {
            axis,
            ratio,
            first,
            second,
        } = tree
        else {
            panic!("expected root split");
        };
        assert_eq!(axis, SplitAxis::Horizontal);
        // Master takes 0.6 on the right → the stack region is first at 0.4.
        assert!((ratio - 0.4).abs() < 1e-6);
        assert_eq!(second.as_ref(), &LayoutTree::Leaf(TileId::new(1)));
        // The two stack tiles sit side-by-side left-to-right.
        let LayoutTree::Split {
            axis: stack_axis,
            first: s1,
            second: s2,
            ..
        } = first.as_ref()
        else {
            panic!("expected stack split");
        };
        assert_eq!(*stack_axis, SplitAxis::Horizontal);
        assert_eq!(s1.as_ref(), &LayoutTree::Leaf(TileId::new(2)));
        assert_eq!(s2.as_ref(), &LayoutTree::Leaf(TileId::new(3)));
    }

    #[test]
    fn compute_layout_master_count_two_arranges_vertically() {
        let tiling = TilingSpec {
            master: MasterSpec {
                count: 2,
                ..MasterSpec::default()
            },
            ..TilingSpec::default()
        };
        let tree = compute_layout(&tiling, &ids(&[1, 2, 3])).unwrap();
        let LayoutTree::Split { first, second, .. } = tree else {
            panic!("expected root split");
        };
        // Stack region (1 tile) first, master region second (side right).
        assert_eq!(first.as_ref(), &LayoutTree::Leaf(TileId::new(3)));
        let LayoutTree::Split {
            axis: master_axis,
            first: m1,
            second: m2,
            ..
        } = second.as_ref()
        else {
            panic!("expected master split");
        };
        // Vertical arrangement: stacked top-to-bottom.
        assert_eq!(*master_axis, SplitAxis::Vertical);
        assert_eq!(m1.as_ref(), &LayoutTree::Leaf(TileId::new(1)));
        assert_eq!(m2.as_ref(), &LayoutTree::Leaf(TileId::new(2)));
    }

    #[test]
    fn compute_layout_master_side_left_puts_master_first() {
        let tiling = TilingSpec {
            master: MasterSpec {
                side: SideSpec::Left,
                ..MasterSpec::default()
            },
            ..TilingSpec::default()
        };
        let tree = compute_layout(&tiling, &ids(&[1, 2])).unwrap();
        let LayoutTree::Split { ratio, first, .. } = tree else {
            panic!("expected root split");
        };
        assert!((ratio - 0.6).abs() < 1e-6);
        assert_eq!(first.as_ref(), &LayoutTree::Leaf(TileId::new(1)));
    }

    #[test]
    fn compute_layout_all_master_when_count_covers_tiles() {
        let tiling = TilingSpec {
            master: MasterSpec {
                count: 5,
                ..MasterSpec::default()
            },
            ..TilingSpec::default()
        };
        let tree = compute_layout(&tiling, &ids(&[1, 2])).unwrap();
        // No stack region: only the master arrangement.
        let LayoutTree::Split { axis, .. } = tree else {
            panic!("expected master split");
        };
        assert_eq!(axis, SplitAxis::Vertical);
    }

    #[test]
    fn add_tile_appends_to_end_and_focuses() {
        let mut ws = workspace_with(2);
        let order_before = ws.tile_ids();
        let new_id = ws.add_tile(bound("new"));
        let order = ws.tile_ids();
        assert_eq!(order.len(), 3);
        assert_eq!(order[..2], order_before[..]);
        assert_eq!(*order.last().unwrap(), new_id, "insert: end appends");
        assert_eq!(ws.focused_tile, new_id, "new tile takes focus");
        assert!(matches!(
            ws.tiles.get(&new_id).map(|t| &t.local),
            Some(ViewLocalState::GenericList {
                cursor: 0,
                scroll: 0
            })
        ));
    }

    #[test]
    fn first_added_tile_takes_the_full_center() {
        let mut ws = Workspace::from_tiling(TilingSpec::default(), Vec::new());
        assert!(ws.is_home());
        assert!(ws.layout().is_none());
        let id = ws.add_tile(bound("solo"));
        assert!(!ws.is_home());
        assert_eq!(ws.layout(), Some(LayoutTree::Leaf(id)));
    }

    #[test]
    fn close_tile_keeps_order_and_refocuses_neighbor() {
        let mut ws = workspace_with(3);
        let order = ws.tile_ids();
        ws.focused_tile = order[1];
        assert!(ws.close_tile(order[1]));
        assert_eq!(ws.tile_ids(), vec![order[0], order[2]]);
        assert_eq!(
            ws.focused_tile, order[2],
            "focus moves to the next in order"
        );
        assert!(!ws.tiles.contains_key(&order[1]));
    }

    #[test]
    fn closing_last_tile_returns_home() {
        let mut ws = workspace_with(1);
        let only = ws.tile_ids()[0];
        assert!(ws.close_tile(only));
        assert!(ws.is_home());
        assert!(ws.layout().is_none());
    }

    #[test]
    fn close_tile_ignores_unknown_tile() {
        let mut ws = workspace_with(3);
        assert!(!ws.close_tile(TileId::new(999)));
        assert_eq!(ws.tile_ids().len(), 3);
    }

    #[test]
    fn focus_next_cycles_center_order() {
        let mut ws = workspace_with(3);
        let order = ws.tile_ids();
        assert_eq!(ws.focused_tile, order[0]);
        ws.focus_next();
        assert_eq!(ws.focused_tile, order[1]);
        ws.focus_next();
        assert_eq!(ws.focused_tile, order[2]);
        ws.focus_next();
        assert_eq!(ws.focused_tile, order[0]);
        ws.focus_prev();
        assert_eq!(ws.focused_tile, order[2]);
    }

    #[test]
    fn move_tile_reorders_with_wrap() {
        let mut ws = workspace_with(3);
        let order = ws.tile_ids();
        ws.focused_tile = order[0];
        assert!(ws.move_focused_in_stack(1));
        assert_eq!(ws.tile_ids(), vec![order[1], order[0], order[2]]);
        assert!(ws.move_focused_in_stack(-1));
        assert_eq!(ws.tile_ids(), order);
        assert!(ws.move_focused_in_stack(-1));
        assert_eq!(ws.tile_ids(), vec![order[1], order[2], order[0]]);
    }

    #[test]
    fn zoom_promotes_to_master_and_swaps_at_front() {
        let mut ws = workspace_with(3);
        let order = ws.tile_ids();
        assert!(ws.zoom_tile(order[2]));
        assert_eq!(ws.tile_ids(), vec![order[2], order[0], order[1]]);
        assert_eq!(ws.focused_tile, order[2]);
        // Zooming the leader swaps it with the runner-up.
        assert!(ws.zoom_tile(order[2]));
        assert_eq!(ws.tile_ids(), vec![order[0], order[2], order[1]]);
    }

    #[test]
    fn resize_master_moves_boundary_in_screen_space() {
        let mut ws = workspace_with(2);
        let before = ws.tiling.master.ratio;
        // Master defaults to the right: moving the boundary left grows it.
        assert!(ws.resize_master(FocusDirection::Left));
        assert!(ws.tiling.master.ratio > before);
        assert!(ws.resize_master(FocusDirection::Right));
        assert!((ws.tiling.master.ratio - before).abs() < 1e-6);
        assert!(!ws.resize_master(FocusDirection::Up));
    }

    #[test]
    fn focus_in_direction_uses_computed_geometry() {
        // Two tiles: first is master (right), second is stack (left).
        let mut ws = workspace_with(2);
        let order = ws.tile_ids();
        ws.focused_tile = order[0];
        assert!(ws.focus_in_direction(FocusDirection::Left));
        assert_eq!(ws.focused_tile, order[1]);
        assert!(ws.focus_in_direction(FocusDirection::Right));
        assert_eq!(ws.focused_tile, order[0]);
    }
}
