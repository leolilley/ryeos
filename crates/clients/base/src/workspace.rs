//! Workspace — layout tree, tile state, focus, input bar.

use crate::ids::{ThreadId, TileId};
use crate::layout::{layout_rects, LayoutTree, Rect, SplitAxis};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// View specs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ViewSpec {
    Thread { thread_id: Option<ThreadId> },
    ThreadList,
    Overview,
    Remotes,
    Services,
    ItemInspector,
    Schedules,
    GcStatus,
    Files,
    Projects,
    SpaceBrowser { project: Option<String> },
    Trust,
    Graph { graph_id: Option<String> },
    EventInspector,
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
    Thread(ThreadViewState),
    ThreadList {
        cursor: usize,
        filter: String,
    },
    SpaceBrowser {
        cursor: usize,
        query: String,
        kind: String,
        scroll: usize,
    },
    Files {
        root: String,
        path: String,
        cursor: usize,
        scroll: usize,
    },
    GenericList {
        cursor: usize,
        scroll: usize,
    },
    #[default]
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadViewState {
    pub mode: ThreadViewMode,
    pub timeline_cursor: usize,
    pub timeline_scroll: usize,
    pub expanded_turns: HashSet<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ThreadViewMode {
    Timeline,
    Detail,
}

impl Default for ThreadViewState {
    fn default() -> Self {
        Self {
            mode: ThreadViewMode::Timeline,
            timeline_cursor: 0,
            timeline_scroll: 0,
            expanded_turns: HashSet::new(),
        }
    }
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
            ViewSpec::Thread { .. } => ViewLocalState::Thread(ThreadViewState::default()),
            ViewSpec::ThreadList => ViewLocalState::ThreadList {
                cursor: 0,
                filter: String::new(),
            },
            ViewSpec::SpaceBrowser { .. } => ViewLocalState::SpaceBrowser {
                cursor: 0,
                query: String::new(),
                kind: String::new(),
                scroll: 0,
            },
            ViewSpec::Files => ViewLocalState::Files {
                root: "project_ai".to_string(),
                path: String::new(),
                cursor: 0,
                scroll: 0,
            },
            ViewSpec::Services
            | ViewSpec::ItemInspector
            | ViewSpec::Schedules
            | ViewSpec::GcStatus
            | ViewSpec::Projects
            | ViewSpec::EventInspector => ViewLocalState::GenericList {
                cursor: 0,
                scroll: 0,
            },
            ViewSpec::Overview | ViewSpec::Remotes | ViewSpec::Trust | ViewSpec::Graph { .. } => {
                ViewLocalState::None
            }
        }
    }

    pub fn input_capability(&self) -> InputCapability {
        match self {
            ViewSpec::Thread { .. } => InputCapability::Prompt,
            ViewSpec::ThreadList | ViewSpec::SpaceBrowser { .. } => InputCapability::Filter,
            ViewSpec::EventInspector => InputCapability::Filter,
            ViewSpec::Files => InputCapability::Filter,
            ViewSpec::Overview
            | ViewSpec::Remotes
            | ViewSpec::Services
            | ViewSpec::ItemInspector
            | ViewSpec::Schedules
            | ViewSpec::GcStatus
            | ViewSpec::Projects
            | ViewSpec::Trust
            | ViewSpec::Graph { .. } => InputCapability::None,
        }
    }

    /// Human-readable label for the input context hint.
    pub fn input_hint(&self) -> &'static str {
        match self {
            ViewSpec::Thread { thread_id } => {
                if thread_id.is_some() {
                    "thread"
                } else {
                    "new thread"
                }
            }
            ViewSpec::ThreadList => "threads filter",
            ViewSpec::Overview => "overview",
            ViewSpec::Remotes => "remotes",
            ViewSpec::Services => "services",
            ViewSpec::ItemInspector => "item inspector",
            ViewSpec::Schedules => "schedules",
            ViewSpec::GcStatus => "gc status",
            ViewSpec::Files => "files",
            ViewSpec::Projects => "projects",
            ViewSpec::SpaceBrowser { .. } => "search items",
            ViewSpec::Trust => "trust",
            ViewSpec::Graph { .. } => "graph",
            ViewSpec::EventInspector => "events filter",
        }
    }

    /// Short title for tile header.
    pub fn title(&self) -> String {
        match self {
            ViewSpec::Thread { thread_id } => {
                if let Some(id) = thread_id {
                    format!("Thread {}", id.0)
                } else {
                    "New Thread".into()
                }
            }
            ViewSpec::ThreadList => "Threads".into(),
            ViewSpec::Overview => "Overview".into(),
            ViewSpec::Remotes => "Remotes".into(),
            ViewSpec::Services => "Services".into(),
            ViewSpec::ItemInspector => "Item".into(),
            ViewSpec::Schedules => "Schedules".into(),
            ViewSpec::GcStatus => "GC".into(),
            ViewSpec::Files => "Files".into(),
            ViewSpec::Projects => "Projects".into(),
            ViewSpec::SpaceBrowser { .. } => "Items".into(),
            ViewSpec::Trust => "Trust".into(),
            ViewSpec::Graph { graph_id } => {
                if let Some(id) = graph_id {
                    format!("Graph: {}", id)
                } else {
                    "Graph".into()
                }
            }
            ViewSpec::EventInspector => "Events".into(),
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
// Input bar
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InputBarState {
    pub text: String,
    pub cursor: usize,
    pub history: Vec<String>,
    pub history_index: Option<usize>,
}

impl InputBarState {
    pub fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let prev = self.text[..self.cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.text.drain(prev..self.cursor);
            self.cursor = prev;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            let next = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
            self.text.drain(self.cursor..next);
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.text[..self.cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.text.len();
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn submit(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        if !text.is_empty() {
            self.history.push(text.clone());
        }
        self.cursor = 0;
        self.history_index = None;
        text
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = self.history_index.unwrap_or(self.history.len());
        if idx > 0 {
            self.history_index = Some(idx - 1);
            self.text = self.history[idx - 1].clone();
            self.cursor = self.text.len();
        }
    }

    pub fn history_next(&mut self) {
        if let Some(idx) = self.history_index {
            if idx + 1 < self.history.len() {
                self.history_index = Some(idx + 1);
                self.text = self.history[idx + 1].clone();
                self.cursor = self.text.len();
            } else {
                self.history_index = None;
                self.text.clear();
                self.cursor = 0;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Workspace
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub layout: LayoutTree,
    pub tiles: HashMap<TileId, TileState>,
    pub focused_tile: TileId,
    pub input_bar: InputBarState,
}

impl Workspace {
    /// Create default 3-pane workspace.
    pub fn default_three_pane() -> Self {
        let list_id = TileId::new(1);
        let thread_id = TileId::new(2);
        let status_id = TileId::new(3);

        let layout = LayoutTree::default_three_pane(list_id, thread_id, status_id);

        let mut tiles = HashMap::new();
        tiles.insert(
            list_id,
            TileState {
                view: ViewSpec::ThreadList,
                local: ViewLocalState::ThreadList {
                    cursor: 0,
                    filter: String::new(),
                },
            },
        );
        tiles.insert(
            thread_id,
            TileState {
                view: ViewSpec::Thread { thread_id: None },
                local: ViewLocalState::Thread(ThreadViewState::default()),
            },
        );
        tiles.insert(
            status_id,
            TileState {
                view: ViewSpec::Overview,
                local: ViewLocalState::GenericList {
                    cursor: 0,
                    scroll: 0,
                },
            },
        );

        Self {
            layout,
            tiles,
            focused_tile: thread_id,
            input_bar: InputBarState::default(),
        }
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

    pub fn is_home(&self) -> bool {
        let ids = self.layout.tile_ids();
        ids.len() == 1
            && self
                .tiles
                .get(&ids[0])
                .is_some_and(|tile| matches!(tile.view, ViewSpec::Graph { graph_id: None }))
    }

    pub fn reset_to_home(&mut self) {
        let tile_id = self
            .layout
            .tile_ids()
            .into_iter()
            .next()
            .or_else(|| self.tiles.keys().copied().next())
            .unwrap_or_else(|| TileId::new(1));
        self.layout = LayoutTree::Leaf(tile_id);
        self.tiles.clear();
        let view = ViewSpec::Graph { graph_id: None };
        self.tiles.insert(
            tile_id,
            TileState {
                local: view.initial_local_state(),
                view,
            },
        );
        self.focused_tile = tile_id;
    }

    pub fn replace_focused_view(&mut self, view: ViewSpec) -> Option<TileId> {
        let tile = self.tiles.get_mut(&self.focused_tile)?;
        tile.local = view.initial_local_state();
        tile.view = view;
        Some(self.focused_tile)
    }

    /// Focus next tile in layout order.
    pub fn focus_next(&mut self) {
        let ids = self.layout.tile_ids();
        if let Some(pos) = ids.iter().position(|id| *id == self.focused_tile) {
            let next = (pos + 1) % ids.len();
            self.focused_tile = ids[next];
        }
    }

    /// Focus previous tile in layout order.
    pub fn focus_prev(&mut self) {
        let ids = self.layout.tile_ids();
        if let Some(pos) = ids.iter().position(|id| *id == self.focused_tile) {
            let prev = if pos == 0 { ids.len() - 1 } else { pos - 1 };
            self.focused_tile = ids[prev];
        }
    }

    pub fn focus_in_direction(&mut self, direction: FocusDirection) -> bool {
        let rects = layout_rects(&self.layout, Rect::new(0, 0, 10_000, 10_000));
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

    /// Allocate a fresh TileId.
    fn next_tile_id() -> TileId {
        static COUNTER: AtomicU64 = AtomicU64::new(100);
        TileId::new(COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    /// Split the focused tile along the given axis.
    /// The existing tile becomes `first`, a new tile is `second`.
    /// Returns the new TileId.
    pub fn split_focused(&mut self, axis: SplitAxis, new_view: ViewSpec) -> Option<TileId> {
        let focused = self.focused_tile;
        let new_id = Self::next_tile_id();

        let new_layout = replace_leaf_with_split(&self.layout, focused, axis, new_id)?;

        self.layout = new_layout;
        self.tiles.insert(
            new_id,
            TileState {
                local: new_view.initial_local_state(),
                view: new_view,
            },
        );
        Some(new_id)
    }

    /// Add a tile using a dwm-style master/stack layout.
    /// The first existing tile remains master; all other tiles form a vertical stack.
    pub fn add_master_stack_tile(&mut self, new_view: ViewSpec) -> Option<TileId> {
        let mut ids = self.layout.tile_ids();
        if ids.is_empty() {
            return None;
        }
        let new_id = Self::next_tile_id();
        ids.push(new_id);
        self.layout = master_stack_layout(&ids)?;
        self.tiles.insert(
            new_id,
            TileState {
                local: new_view.initial_local_state(),
                view: new_view,
            },
        );
        Some(new_id)
    }

    /// Close the focused tile. If it's the last tile, do nothing.
    /// Returns to the previous tile in order.
    pub fn close_focused(&mut self) {
        self.close_tile(self.focused_tile);
    }

    /// Close a tile by id. If it's the last tile or not present in the layout,
    /// do nothing and return false.
    pub fn close_tile(&mut self, tile_id: TileId) -> bool {
        if self.layout.tile_ids().len() <= 1 {
            return false;
        }

        let Some((new_tree, true)) = remove_leaf(&self.layout, tile_id) else {
            return false;
        };

        self.layout = new_tree;
        self.tiles.remove(&tile_id);

        let remaining = self.layout.tile_ids();
        self.focused_tile = remaining.first().copied().unwrap_or(tile_id);
        true
    }

    pub fn close_tile_master_stack(&mut self, tile_id: TileId) -> bool {
        if self.layout.tile_ids().len() <= 1 {
            return false;
        }
        if !self.tiles.contains_key(&tile_id) {
            return false;
        }
        let remaining: Vec<_> = self
            .layout
            .tile_ids()
            .into_iter()
            .filter(|id| *id != tile_id)
            .collect();
        let Some(layout) = master_stack_layout(&remaining) else {
            return false;
        };
        self.layout = layout;
        self.tiles.remove(&tile_id);
        self.focused_tile = remaining.first().copied().unwrap_or(tile_id);
        true
    }

    /// Reset layout to the default 3-pane.
    pub fn reset_layout(&mut self) {
        let default = Self::default_three_pane();
        self.layout = default.layout;
        self.tiles = default.tiles;
        self.focused_tile = default.focused_tile;
    }

    /// Move cursor up in the focused list view.
    pub fn cursor_up(&mut self) {
        if let Some(tile) = self.tiles.get_mut(&self.focused_tile) {
            match &mut tile.local {
                ViewLocalState::ThreadList { cursor, .. } if *cursor > 0 => {
                    *cursor -= 1;
                }
                ViewLocalState::SpaceBrowser { cursor, .. } if *cursor > 0 => {
                    *cursor -= 1;
                }
                ViewLocalState::Files { cursor, .. } if *cursor > 0 => {
                    *cursor -= 1;
                }
                ViewLocalState::GenericList { cursor, .. } if *cursor > 0 => {
                    *cursor -= 1;
                }
                ViewLocalState::Thread(state) if state.timeline_cursor > 0 => {
                    state.timeline_cursor -= 1;
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
                ViewLocalState::ThreadList { cursor, .. }
                    if *cursor < total_items.saturating_sub(1) =>
                {
                    *cursor += 1;
                }
                ViewLocalState::SpaceBrowser { cursor, .. }
                    if *cursor < total_items.saturating_sub(1) =>
                {
                    *cursor += 1;
                }
                ViewLocalState::Files { cursor, .. } if *cursor < total_items.saturating_sub(1) => {
                    *cursor += 1;
                }
                ViewLocalState::GenericList { cursor, .. }
                    if *cursor < total_items.saturating_sub(1) =>
                {
                    *cursor += 1;
                }
                ViewLocalState::Thread(state)
                    if state.timeline_cursor < total_items.saturating_sub(1) =>
                {
                    state.timeline_cursor += 1;
                }
                _ => {}
            }
        }
    }
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

fn master_stack_layout(ids: &[TileId]) -> Option<LayoutTree> {
    match ids {
        [] => None,
        [only] => Some(LayoutTree::Leaf(*only)),
        [master, stack @ ..] => Some(LayoutTree::Split {
            axis: SplitAxis::Vertical,
            ratio: 0.58,
            first: Box::new(LayoutTree::Leaf(*master)),
            second: Box::new(stack_layout(stack)?),
        }),
    }
}

fn stack_layout(ids: &[TileId]) -> Option<LayoutTree> {
    match ids {
        [] => None,
        [only] => Some(LayoutTree::Leaf(*only)),
        [first, rest @ ..] => Some(LayoutTree::Split {
            axis: SplitAxis::Horizontal,
            ratio: 1.0 / ids.len() as f32,
            first: Box::new(LayoutTree::Leaf(*first)),
            second: Box::new(stack_layout(rest)?),
        }),
    }
}

// ---------------------------------------------------------------------------
// Tree manipulation helpers
// ---------------------------------------------------------------------------

/// Replace a leaf node with a split containing the original leaf + a new leaf.
fn replace_leaf_with_split(
    tree: &LayoutTree,
    target: TileId,
    axis: SplitAxis,
    new_id: TileId,
) -> Option<LayoutTree> {
    match tree {
        LayoutTree::Leaf(id) if *id == target => Some(LayoutTree::Split {
            axis,
            ratio: 0.5,
            first: Box::new(LayoutTree::Leaf(target)),
            second: Box::new(LayoutTree::Leaf(new_id)),
        }),
        LayoutTree::Leaf(_) => None,
        LayoutTree::Split {
            axis: a,
            ratio,
            first,
            second,
        } => {
            if let Some(new_first) = replace_leaf_with_split(first, target, axis, new_id) {
                Some(LayoutTree::Split {
                    axis: *a,
                    ratio: *ratio,
                    first: Box::new(new_first),
                    second: second.clone(),
                })
            } else {
                replace_leaf_with_split(second, target, axis, new_id).map(|new_second| {
                    LayoutTree::Split {
                        axis: *a,
                        ratio: *ratio,
                        first: first.clone(),
                        second: Box::new(new_second),
                    }
                })
            }
        }
    }
}

/// Remove a leaf from the tree, collapsing its parent split node.
/// Returns the collapsed tree, or None if the leaf wasn't found.
fn remove_leaf(tree: &LayoutTree, target: TileId) -> Option<(LayoutTree, bool)> {
    match tree {
        LayoutTree::Leaf(id) if *id == target => {
            // Can't remove root leaf
            None
        }
        LayoutTree::Leaf(id) => Some((LayoutTree::Leaf(*id), false)),
        LayoutTree::Split { first, second, .. } => {
            match (first.as_ref(), second.as_ref()) {
                (LayoutTree::Leaf(a), LayoutTree::Leaf(b)) => {
                    if *a == target {
                        // Remove first, keep second
                        Some((*second.clone(), true))
                    } else if *b == target {
                        // Remove second, keep first
                        Some((*first.clone(), true))
                    } else {
                        // Neither child is the target — recurse
                        Some((
                            LayoutTree::Split {
                                axis: split_axis(tree),
                                ratio: split_ratio(tree),
                                first: first.clone(),
                                second: second.clone(),
                            },
                            false,
                        ))
                    }
                }
                _ => {
                    // Try removing from first
                    if let Some((new_first, true)) = remove_leaf(first, target) {
                        return Some((
                            LayoutTree::Split {
                                axis: split_axis(tree),
                                ratio: split_ratio(tree),
                                first: Box::new(new_first),
                                second: second.clone(),
                            },
                            true,
                        ));
                    }
                    // Try removing from second
                    if let Some((new_second, true)) = remove_leaf(second, target) {
                        return Some((
                            LayoutTree::Split {
                                axis: split_axis(tree),
                                ratio: split_ratio(tree),
                                first: first.clone(),
                                second: Box::new(new_second),
                            },
                            true,
                        ));
                    }
                    Some((
                        LayoutTree::Split {
                            axis: split_axis(tree),
                            ratio: split_ratio(tree),
                            first: first.clone(),
                            second: second.clone(),
                        },
                        false,
                    ))
                }
            }
        }
    }
}

fn split_axis(tree: &LayoutTree) -> SplitAxis {
    match tree {
        LayoutTree::Split { axis, .. } => *axis,
        LayoutTree::Leaf(_) => unreachable!("split_axis called on leaf"),
    }
}

fn split_ratio(tree: &LayoutTree) -> f32 {
    match tree {
        LayoutTree::Split { ratio, .. } => *ratio,
        LayoutTree::Leaf(_) => unreachable!("split_ratio called on leaf"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_default_has_three_tiles() {
        let ws = Workspace::default_three_pane();
        assert_eq!(ws.tiles.len(), 3);
        assert_eq!(ws.focused_tile, TileId::new(2)); // thread tile focused
    }

    #[test]
    fn workspace_focus_next_cycles() {
        let mut ws = Workspace::default_three_pane();
        assert_eq!(ws.focused_tile, TileId::new(2));
        ws.focus_next();
        assert_eq!(ws.focused_tile, TileId::new(3));
        ws.focus_next();
        assert_eq!(ws.focused_tile, TileId::new(1));
        ws.focus_next();
        assert_eq!(ws.focused_tile, TileId::new(2));
    }

    #[test]
    fn workspace_focus_prev_cycles() {
        let mut ws = Workspace::default_three_pane();
        ws.focus_prev();
        assert_eq!(ws.focused_tile, TileId::new(1));
    }

    #[test]
    fn input_bar_editing() {
        let mut bar = InputBarState::default();
        bar.insert_char('h');
        bar.insert_char('i');
        assert_eq!(bar.text, "hi");
        assert_eq!(bar.cursor, 2);

        bar.move_left();
        assert_eq!(bar.cursor, 1);
        bar.backspace();
        assert_eq!(bar.text, "i");
        assert_eq!(bar.cursor, 0);

        bar.insert_char('h');
        assert_eq!(bar.text, "hi");
    }

    #[test]
    fn input_bar_submit_adds_history() {
        let mut bar = InputBarState::default();
        bar.text = "hello".into();
        bar.cursor = 5;
        let submitted = bar.submit();
        assert_eq!(submitted, "hello");
        assert_eq!(bar.history.len(), 1);
        assert!(bar.text.is_empty());
    }

    #[test]
    fn close_focused_preserves_nested_siblings() {
        let mut ws = Workspace::default_three_pane();
        ws.focused_tile = TileId::new(2);
        ws.close_focused();

        let ids = ws.layout.tile_ids();
        assert_eq!(ids, vec![TileId::new(1), TileId::new(3)]);
        assert!(ws.tiles.contains_key(&TileId::new(1)));
        assert!(ws.tiles.contains_key(&TileId::new(3)));
        assert!(!ws.tiles.contains_key(&TileId::new(2)));
    }

    #[test]
    fn split_focused_initializes_view_local_state() {
        let mut ws = Workspace::default_three_pane();
        let new_id = ws
            .split_focused(
                SplitAxis::Horizontal,
                ViewSpec::SpaceBrowser { project: None },
            )
            .expect("split should succeed");

        assert!(matches!(
            ws.tiles.get(&new_id).map(|tile| &tile.local),
            Some(ViewLocalState::SpaceBrowser { cursor: 0, query, kind, scroll: 0 })
                if query.is_empty() && kind.is_empty()
        ));
    }

    #[test]
    fn split_focused_does_not_orphan_tile_when_focus_missing_from_layout() {
        let mut ws = Workspace::default_three_pane();
        ws.focused_tile = TileId::new(999);
        let before = ws.tiles.len();

        assert!(ws
            .split_focused(SplitAxis::Horizontal, ViewSpec::Services)
            .is_none());
        assert_eq!(ws.tiles.len(), before);
        assert_eq!(
            ws.layout.tile_ids(),
            vec![TileId::new(1), TileId::new(2), TileId::new(3)]
        );
    }

    #[test]
    fn close_tile_ignores_tile_missing_from_layout() {
        let mut ws = Workspace::default_three_pane();
        let before_len = ws.tiles.len();

        assert!(!ws.close_tile(TileId::new(999)));
        assert_eq!(ws.tiles.len(), before_len);
        assert!(ws.tiles.contains_key(&TileId::new(1)));
        assert!(ws.tiles.contains_key(&TileId::new(2)));
        assert!(ws.tiles.contains_key(&TileId::new(3)));
        assert_eq!(
            ws.layout.tile_ids(),
            vec![TileId::new(1), TileId::new(2), TileId::new(3)]
        );
    }
}
