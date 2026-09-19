//! ViewSet — canonical layout tree, view state and focus.
//!
//! The tree owns placement. The surface's tiling recipe seeds an arrangement;
//! it must never flatten a nested layout on an ordinary open/close/move edit.
//! Traversal order is derived, not a second mutable placement authority.

use crate::ids::{RyeOsViewInstanceKey, TileId, ViewGroupId};
use crate::layout::{LayoutTree, Rect, SplitAxis, layout_rects};
use crate::surface::{ArrangeSpec, SideSpec, TilingModeSpec, TilingSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// View specs
// ---------------------------------------------------------------------------

/// A center tile: a `view:` item ref (views-as-content). Every product
/// concept renders through this — graph/atlas included, via their
/// `widget:`. Tiles are bindings, not code; this is the one uniform
/// content form. The engine never names a specific view ref: which views
/// exist is a content/surface concern, not engine code.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewSpec {
    pub view_ref: String,
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

/// One transient row-change flash: when it happened and, when the change
/// crossed a tone boundary (created→running, →completed, →failed) or the
/// row is newly arrived, which tone to flash in. `None` = a content-only
/// change; renderers flash their default accent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowFlash {
    pub at_ms: u64,
    #[serde(default)]
    pub tone: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FieldEventRefState {
    pub chain_root_id: String,
    pub chain_seq: u64,
    pub event_hash: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum FieldCursorState {
    #[default]
    Live,
    BraidCut {
        anchor: FieldEventRefState,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldPlaybackState {
    pub playing: bool,
    #[serde(default)]
    pub awaiting: Option<FieldEventRefState>,
}

/// Shared replay state for all field instances declaring one signed cursor
/// scope on the mounted surface. Per-view local cursor values are mirrors for
/// VM composition; this is the authoritative transition/fencing state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldCursorScopeState {
    pub cursor: FieldCursorState,
    pub playback: FieldPlaybackState,
    #[serde(default)]
    pub subject_fingerprint: String,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub pending_source_keys: BTreeSet<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldExpansionState {
    pub max_depth: u16,
    pub max_entities: u32,
    #[serde(default)]
    pub continuation_token: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldFingerprintState {
    pub id: String,
    pub fact_kind: String,
    pub fingerprint: String,
    pub status: Option<String>,
    pub label: Option<String>,
    pub tone: Option<String>,
    #[serde(default)]
    pub traits: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldChangeState {
    pub id: String,
    pub kind: String,
    pub at_ms: u64,
    pub tone: Option<String>,
    pub prior_fingerprint: Option<String>,
    pub fingerprint: Option<String>,
    pub tombstone_label: Option<String>,
    #[serde(default)]
    pub tombstone_traits: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FieldLocalState {
    pub selected: Option<String>,
    pub collapsed_groups: BTreeSet<String>,
    pub hidden_layers: BTreeSet<String>,
    pub compare: Vec<String>,
    pub cursor: FieldCursorState,
    pub playback: FieldPlaybackState,
    pub query: String,
    pub search_match: Option<String>,
    pub expansions: BTreeMap<String, FieldExpansionState>,
    pub change_fingerprints: BTreeMap<String, FieldFingerprintState>,
    pub changes: BTreeMap<String, FieldChangeState>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum ViewLocalState {
    GenericList {
        cursor: usize,
        scroll: usize,
        /// Feed sections (turns) the operator has folded shut, by section
        /// index. Only the timeline lens uses this; row lists leave it empty.
        #[serde(default)]
        collapsed: BTreeSet<usize>,
        /// Rows expanded in place, keyed by stable record identity. Tables and
        /// rows widgets use this; feeds ignore it.
        #[serde(default)]
        expanded_rows: BTreeSet<String>,
        /// Hierarchy rows folded shut, keyed by the hierarchy's stable authored
        /// id. Separate from detail expansion: a tree row can collapse its
        /// descendants while still exposing its own expanded detail.
        #[serde(default)]
        collapsed_tree_rows: BTreeSet<String>,
        /// Rows whose projected content changed recently, keyed by stable
        /// record identity. Renderers use this as a transient flash signal;
        /// the flash carries the transition's tone when there was one.
        #[serde(default)]
        changed_rows: BTreeMap<String, RowFlash>,
    },
    Field(FieldLocalState),
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
    /// Bind a `view:` ref.
    pub fn bound(view_ref: impl Into<String>) -> Self {
        Self {
            view_ref: view_ref.into(),
        }
    }

    pub fn initial_local_state(&self) -> ViewLocalState {
        // Every bound tile gets list-local state; for the scene widgets
        // (graph/atlas) the cursor is simply unused.
        ViewLocalState::GenericList {
            cursor: 0,
            scroll: 0,
            collapsed: BTreeSet::new(),
            expanded_rows: BTreeSet::new(),
            collapsed_tree_rows: BTreeSet::new(),
            changed_rows: BTreeMap::new(),
        }
    }

    pub fn input_capability(&self) -> InputCapability {
        InputCapability::None
    }

    /// Short title for tile header — the trailing segment of the ref.
    pub fn title(&self) -> String {
        self.view_ref
            .rsplit('/')
            .next()
            .unwrap_or(&self.view_ref)
            .to_string()
    }
}

// ---------------------------------------------------------------------------
// Tile state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileState {
    pub instance_key: RyeOsViewInstanceKey,
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
        // Single-lens keeps the center at one tile, which master-stack
        // already renders as a full-center monocle; share the layout so a
        // stray extra tile still degrades safely rather than vanishing.
        TilingModeSpec::MasterStack | TilingModeSpec::SingleLens => {
            master_stack_layout(tiling, tiles)
        }
    }
}

fn master_stack_layout(tiling: &TilingSpec, tiles: &[TileId]) -> Option<LayoutTree> {
    match tiles {
        [] => None,
        [only] => Some(LayoutTree::single(*only)),
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
        [only] => Some(LayoutTree::single(*only)),
        _ => {
            // Balanced subdivisions retain equal allocation without producing
            // an arbitrarily deep tree or sub-minimum split ratios.
            let middle = ids.len() / 2;
            Some(LayoutTree::Split {
                axis,
                ratio: middle as f32 / ids.len() as f32,
                first: Box::new(arrange_region(&ids[..middle], arrange)?),
                second: Box::new(arrange_region(&ids[middle..], arrange)?),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// ViewSet
// ---------------------------------------------------------------------------

/// One frame on the lens stack: the view a step-in left behind, plus the
/// seat-facet context it read. Popping re-appends the captured facets
/// (last-writer-wins over the seat log, so no history rewrite) and restores
/// the view — the "return" half of the debugger step-in. Frames are recorded
/// on single-lens surfaces, where the center is swapped in place, so the stack
/// is the only record of where a drill came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LensFrame {
    /// The view the step-in replaced.
    pub view: ViewSpec,
    /// Snapshot of the seat facet fold at push time, keyed by facet name. On
    /// pop, any facet whose current value differs is re-appended to this value,
    /// restoring the braid/selection context the leaving view was reading.
    pub facets: BTreeMap<String, Value>,
    /// Human label for the level this frame represents (the cognition/thread it
    /// was showing — e.g. `study`), for the breadcrumb. `None` falls back to the
    /// view's title. Because a single-lens braid shows *which* execution via a
    /// facet (not the view), this label is what makes `threads ▸ ar25 ▸ study`
    /// legible instead of `threads ▸ timeline ▸ timeline`.
    #[serde(default)]
    pub label: Option<String>,
    /// Effective selection subject of the lens being left. This runtime-only
    /// frame state lets pop restore the prior lens without silently attaching
    /// it to whatever selection the drilled lens currently follows.
    #[serde(default)]
    pub attachment: Option<crate::ui::attachment::SelectionAttachment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewSet {
    /// Presentation identity, independent of tab order or authored label.
    pub id: crate::ids::ViewSetId,
    pub title: String,
    /// Placement and editor ephemera belong to this view set, not to the
    /// shell. Switching view sets must not retarget another view set's draft.
    pub docks: crate::ui::model::RyeOsDockState,
    pub dock_local: BTreeMap<RyeOsViewInstanceKey, ViewLocalState>,
    pub focus_target: Option<crate::ui::model::RyeOsFocusTarget>,
    /// Layout-neutral InputBufferKey values; never execution authority.
    pub input_buffers: BTreeMap<String, crate::ui::model::RyeOsInputState>,
    pub field_query_editing: Option<RyeOsViewInstanceKey>,
    /// The tiling algorithm (from the surface).
    pub tiling: TilingSpec,
    /// Canonical placement authority. Ordering is derived from this tree;
    /// never add a separately mutable ordered tile list alongside it.
    pub root: Option<LayoutTree>,
    /// Per-tile view + local state.
    pub tiles: HashMap<TileId, TileState>,
    /// Focused tile. Dangling when the center is empty.
    pub focused_tile: TileId,
    /// Presentation-only isolation of one mounted tile. The canonical layout
    /// remains intact, so restore is exact and distinct from master promotion.
    pub maximized_tile: Option<TileId>,
    /// Step-in return stack (single-lens surfaces). A drill pushes the view it
    /// left and the facet context it read; a pop restores them. Empty at the
    /// top of the tree. The default is part of the optional lens contract,
    /// not a compatibility path for predecessor view-set schemas.
    #[serde(default)]
    pub lens_stack: Vec<LensFrame>,
    /// Human label for the CURRENT focused level (the cognition/thread stepped
    /// into — e.g. `study`), the tail of the breadcrumb. `None` at the top of
    /// the tree, where the focused view's own title stands. Set on drill,
    /// restored on pop.
    #[serde(default)]
    pub lens_label: Option<String>,
}

impl ViewSet {
    /// Build a view set from a tiling spec and ordered initial views.
    pub fn from_tiling(tiling: TilingSpec, views: Vec<ViewSpec>) -> Self {
        let mut center_tiles = Vec::with_capacity(views.len());
        let mut tiles = HashMap::new();
        for view in views {
            // Initial mounts and later opens share one identity allocator.
            // Reusing 1..N per view set aliases source and input coordinates.
            let id = Self::next_tile_id();
            center_tiles.push(id);
            tiles.insert(
                id,
                TileState {
                    instance_key: RyeOsViewInstanceKey::view_set_tile(id),
                    local: view.initial_local_state(),
                    view,
                },
            );
        }
        let root = compute_layout(&tiling, &center_tiles);
        let focused_tile = center_tiles
            .first()
            .copied()
            .unwrap_or_else(|| TileId::new(0));
        Self {
            id: {
                static COUNTER: AtomicU64 = AtomicU64::new(1);
                crate::ids::ViewSetId::new(COUNTER.fetch_add(1, Ordering::Relaxed))
            },
            title: String::new(),
            docks: crate::ui::model::RyeOsDockState::default(),
            dock_local: BTreeMap::new(),
            focus_target: None,
            input_buffers: BTreeMap::new(),
            field_query_editing: None,
            tiling,
            root,
            tiles,
            focused_tile,
            maximized_tile: None,
            lens_stack: Vec::new(),
            lens_label: None,
        }
    }

    /// Copy only reusable composition. Mounted identities, drafts, transient
    /// observations, focus ephemera and the lens return stack are deliberately
    /// not cloned: those belong to one open set instance.
    pub fn duplicate_composition(&self) -> Self {
        fn duplicate_tree(
            tree: &LayoutTree,
            source: &HashMap<TileId, TileState>,
            destination: &mut HashMap<TileId, TileState>,
        ) -> LayoutTree {
            match tree {
                LayoutTree::Group {
                    label,
                    active,
                    tabs,
                    ..
                } => {
                    let mut next_tabs = Vec::with_capacity(tabs.len());
                    let mut next_active = None;
                    for tile_id in tabs {
                        let source_tile = source
                            .get(tile_id)
                            .expect("validated layout tree references a mounted tile");
                        let next_id = ViewSet::next_tile_id();
                        if tile_id == active {
                            next_active = Some(next_id);
                        }
                        destination.insert(
                            next_id,
                            TileState {
                                instance_key: RyeOsViewInstanceKey::view_set_tile(next_id),
                                local: source_tile.view.initial_local_state(),
                                view: source_tile.view.clone(),
                            },
                        );
                        next_tabs.push(next_id);
                    }
                    LayoutTree::Group {
                        group_id: ViewGroupId::new(next_tabs[0].0),
                        label: label.clone(),
                        active: next_active.expect("layout group has an active mounted tile"),
                        tabs: next_tabs,
                    }
                }
                LayoutTree::Split {
                    axis,
                    ratio,
                    first,
                    second,
                } => LayoutTree::Split {
                    axis: *axis,
                    ratio: *ratio,
                    first: Box::new(duplicate_tree(first, source, destination)),
                    second: Box::new(duplicate_tree(second, source, destination)),
                },
            }
        }

        let mut duplicate = ViewSet::from_tiling(self.tiling.clone(), Vec::new());
        duplicate.title = self.title.clone();
        duplicate.docks = self.docks.clone();
        duplicate.root = self
            .root
            .as_ref()
            .map(|root| duplicate_tree(root, &self.tiles, &mut duplicate.tiles));
        duplicate.focused_tile = duplicate
            .root
            .as_ref()
            .and_then(|root| root.active_tile_ids().first().copied())
            .unwrap_or_else(|| TileId::new(0));
        duplicate.focus_target =
            duplicate
                .root
                .as_ref()
                .map(|_| crate::ui::model::RyeOsFocusTarget::ViewSetTile {
                    tile_id: duplicate.focused_tile.0.to_string(),
                });
        duplicate
    }

    /// Push a return frame: the view a step-in is leaving, the facet context it
    /// read, and the human label of that level. Recorded before the drill's
    /// facet write + center swap so a pop can restore the pre-drill state.
    pub fn push_lens_frame(
        &mut self,
        view: ViewSpec,
        facets: BTreeMap<String, Value>,
        label: Option<String>,
        attachment: Option<crate::ui::attachment::SelectionAttachment>,
    ) {
        self.lens_stack.push(LensFrame {
            view,
            facets,
            label,
            attachment,
        });
    }

    /// Pop the most recent return frame, if any. The caller restores its facets
    /// and view.
    pub fn pop_lens_frame(&mut self) -> Option<LensFrame> {
        self.lens_stack.pop()
    }

    /// Depth of the step-in stack (0 at the top of the tree).
    pub fn lens_depth(&self) -> usize {
        self.lens_stack.len()
    }

    /// A projection of the canonical layout. None when the center is empty.
    pub fn layout(&self) -> Option<LayoutTree> {
        self.root.clone()
    }

    /// Render-only layout. Mutations continue to address `root`; maximising a
    /// tile never installs another placement authority.
    pub fn presentation_layout(&self) -> Option<LayoutTree> {
        self.maximized_tile
            .map(LayoutTree::single)
            .or_else(|| self.root.clone())
    }

    pub fn toggle_maximized(&mut self, tile_id: TileId) -> bool {
        if !self.tiles.contains_key(&tile_id) {
            return false;
        }
        self.maximized_tile = if self.maximized_tile == Some(tile_id) {
            None
        } else {
            Some(tile_id)
        };
        self.focus_tile(tile_id);
        true
    }

    /// An explicit arrange action is the only operation that reconstructs
    /// all geometry from a tiling recipe. Ordinary edits preserve nesting.
    pub fn arrange(&mut self, tiling: TilingSpec) -> bool {
        let root = compute_layout(&tiling, &self.tile_ids());
        if root.as_ref().is_some_and(|tree| tree.validate().is_err()) {
            return false;
        }
        self.root = root;
        self.tiling = tiling;
        true
    }

    /// Relocate an existing view beside another one. Only geometry changes:
    /// the original instance, draft key and local state are retained.
    pub fn move_tile_beside(&mut self, tile: TileId, target: TileId, edge: FocusDirection) -> bool {
        if tile == target {
            return false;
        }
        let Some(root) = &self.root else {
            return false;
        };
        if root.validate().is_err() || !root.tile_ids().contains(&tile) {
            return false;
        }
        let Some(mut next) = root.clone().without_tile(tile) else {
            return false;
        };
        let (axis, before) = match edge {
            FocusDirection::Left => (SplitAxis::Horizontal, true),
            FocusDirection::Right => (SplitAxis::Horizontal, false),
            FocusDirection::Up => (SplitAxis::Vertical, true),
            FocusDirection::Down => (SplitAxis::Vertical, false),
        };
        if !next.split_tile(target, tile, axis, before, 0.5) {
            return false;
        }
        self.root = Some(next);
        self.focus_tile(tile);
        true
    }

    pub fn move_tile_to_group(&mut self, tile: TileId, target: TileId, index: usize) -> bool {
        if !self
            .root
            .as_mut()
            .is_some_and(|tree| tree.move_to_group(tile, target, index))
        {
            return false;
        }
        self.focus_tile(tile);
        true
    }

    /// Transfer a mounted centre view, not a new copy of its definition. Stage
    /// both trees first so a full/deep target leaves the source untouched.
    pub fn move_tile_to_view_set(&mut self, destination: &mut Self, tile: TileId) -> bool {
        if self.id == destination.id || destination.tiles.contains_key(&tile) {
            return false;
        }
        let Some(state) = self.tiles.get(&tile) else {
            return false;
        };
        let instance = state.instance_key.clone();
        if self.input_buffers.keys().any(|key| {
            crate::ui::model::InputBufferKey::storage_key_belongs_to(key, &instance)
                && destination.input_buffers.contains_key(key)
        }) {
            return false;
        }
        let Some(source_root) = self.root.as_ref() else {
            return false;
        };
        if !source_root.tile_ids().contains(&tile) {
            return false;
        }
        let mut target_root = destination.root.clone();
        if let Some(root) = &mut target_root {
            let Some(target) = root.tile_ids().last().copied() else {
                return false;
            };
            let axis = match destination.tiling.stack.arrange {
                ArrangeSpec::Horizontal => SplitAxis::Horizontal,
                ArrangeSpec::Vertical => SplitAxis::Vertical,
            };
            if !root.split_tile(target, tile, axis, false, 0.5) {
                return false;
            }
        } else {
            target_root = Some(LayoutTree::single(tile));
        }
        let next_source = source_root.clone().without_tile(tile);
        let state = self.tiles.remove(&tile).expect("staged mounted view");
        self.root = next_source;
        if self.maximized_tile == Some(tile) {
            self.maximized_tile = None;
        }
        destination.root = target_root;
        destination.tiles.insert(tile, state);
        let keys: Vec<_> = self
            .input_buffers
            .keys()
            .filter(|key| crate::ui::model::InputBufferKey::storage_key_belongs_to(key, &instance))
            .cloned()
            .collect();
        for key in keys {
            destination.input_buffers.insert(
                key.clone(),
                self.input_buffers.remove(&key).expect("collected input"),
            );
        }
        if self.field_query_editing.as_ref() == Some(&instance) {
            destination.field_query_editing = self.field_query_editing.take();
        }
        if self.focused_tile == tile {
            if let Some(next) = self
                .root
                .as_ref()
                .and_then(|root| root.active_tile_ids().first().copied())
            {
                self.focus_tile(next);
            } else {
                self.focused_tile = TileId::new(0);
                self.focus_target = None;
            }
        }
        destination.focus_tile(tile);
        true
    }

    /// Select the containing group as well as focus. Renderers must not
    /// address a hidden tab while the model projects another member.
    pub fn focus_tile(&mut self, tile: TileId) -> bool {
        if !self.root.as_mut().is_some_and(|tree| tree.select_tab(tile)) {
            return false;
        }
        self.focused_tile = tile;
        self.focus_target = Some(crate::ui::model::RyeOsFocusTarget::ViewSetTile {
            tile_id: tile.0.to_string(),
        });
        true
    }

    pub fn cycle_view_tab(&mut self, forward: bool) -> bool {
        let Some(tabs) = self
            .root
            .as_ref()
            .and_then(|tree| tree.group_tabs(self.focused_tile))
        else {
            return false;
        };
        if tabs.len() < 2 {
            return false;
        }
        let Some(index) = tabs.iter().position(|id| *id == self.focused_tile) else {
            return false;
        };
        let target = tabs[wrap_index(index, if forward { 1 } else { -1 }, tabs.len())];
        self.focus_tile(target)
    }

    /// Ordered center tile ids.
    pub fn tile_ids(&self) -> Vec<TileId> {
        self.root
            .as_ref()
            .map(LayoutTree::tile_ids)
            .unwrap_or_default()
    }

    /// An empty center: no tiles. The backdrop scene shows behind the
    /// (empty) center; closing the last tile returns here. There is no
    /// "home" mode — this is a query over the canonical layout.
    pub fn center_is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// Clear the center back to empty.
    pub fn reset_to_empty(&mut self) {
        self.root = None;
        self.tiles.clear();
        self.focused_tile = TileId::new(0);
        self.maximized_tile = None;
        self.focus_target = None;
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
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        TileId::new(COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    /// Open alongside the last region without rearranging existing splits.
    /// Explicit arrange operations, not ordinary insertion, own full reflow.
    pub fn add_tile(&mut self, view: ViewSpec) -> Option<TileId> {
        let id = Self::next_tile_id();
        if let Some(root) = &mut self.root {
            let target = *root.tile_ids().last().expect("nonempty tree");
            let axis = match self.tiling.stack.arrange {
                ArrangeSpec::Horizontal => SplitAxis::Horizontal,
                ArrangeSpec::Vertical => SplitAxis::Vertical,
            };
            if !root.split_tile(target, id, axis, false, 0.5) {
                return None;
            }
        } else {
            self.root = Some(LayoutTree::single(id));
        }
        self.tiles.insert(
            id,
            TileState {
                instance_key: RyeOsViewInstanceKey::view_set_tile(id),
                local: view.initial_local_state(),
                view,
            },
        );
        self.focus_tile(id);
        Some(id)
    }

    /// Close a tile by id, keeping the remaining order. Returns false if
    /// the tile is not in the center. Closing the last tile empties the
    /// center.
    pub fn close_tile(&mut self, tile_id: TileId) -> bool {
        let ids = self.tile_ids();
        let Some(position) = ids.iter().position(|id| *id == tile_id) else {
            return false;
        };
        self.root = self.root.take().and_then(|root| root.without_tile(tile_id));
        self.tiles.remove(&tile_id);
        if self.maximized_tile == Some(tile_id) {
            self.maximized_tile = None;
        }
        if self.focused_tile == tile_id {
            self.focused_tile = self
                .tile_ids()
                .get(position.min(ids.len().saturating_sub(2)))
                .copied()
                .unwrap_or_else(|| TileId::new(0));
            if !self.focus_tile(self.focused_tile) {
                self.focus_target = None;
            }
        }
        true
    }

    /// Close the focused tile.
    pub fn close_focused(&mut self) -> bool {
        self.close_tile(self.focused_tile)
    }

    /// Focus next tile in center order.
    pub fn focus_next(&mut self) {
        let ids = self.tile_ids();
        if let Some(pos) = ids.iter().position(|id| *id == self.focused_tile) {
            let next = (pos + 1) % ids.len();
            self.focus_tile(ids[next]);
        }
    }

    /// Focus previous tile in center order.
    pub fn focus_prev(&mut self) {
        let ids = self.tile_ids();
        if let Some(pos) = ids.iter().position(|id| *id == self.focused_tile) {
            let prev = if pos == 0 { ids.len() - 1 } else { pos - 1 };
            self.focus_tile(ids[prev]);
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
        self.focus_tile(tile_id);
        true
    }

    /// Move a placement in traversal order while preserving nested geometry.
    pub fn move_tile_in_stack(&mut self, tile_id: TileId, delta: i32) -> bool {
        let mut ids = self.tile_ids();
        let len = ids.len();
        if len <= 1 {
            return false;
        }
        let Some(index) = ids.iter().position(|id| *id == tile_id) else {
            return false;
        };
        let new_index = wrap_index(index, delta, len);
        if new_index == index {
            return false;
        }
        let moved = ids.remove(index);
        ids.insert(new_index, moved);
        if !self.root.as_mut().is_some_and(|root| root.reorder(&ids)) {
            return false;
        }
        self.focus_tile(tile_id);
        true
    }

    pub fn move_focused_in_stack(&mut self, delta: i32) -> bool {
        self.move_tile_in_stack(self.focused_tile, delta)
    }

    /// Zoom: promote a tile to the front of the order (into the master
    /// region). If it already leads, swap it with the next tile.
    pub fn zoom_tile(&mut self, tile_id: TileId) -> bool {
        let mut ids = self.tile_ids();
        let len = ids.len();
        if len <= 1 {
            return false;
        }
        let Some(index) = ids.iter().position(|id| *id == tile_id) else {
            return false;
        };
        if index == 0 {
            ids.swap(0, 1);
        } else {
            let moved = ids.remove(index);
            ids.insert(0, moved);
        }
        if !self.root.as_mut().is_some_and(|root| root.reorder(&ids)) {
            return false;
        }
        self.focus_tile(tile_id);
        true
    }

    pub fn zoom_focused(&mut self) -> bool {
        self.zoom_tile(self.focused_tile)
    }

    /// Resize the nearest matching split around the focused region.
    pub fn resize_focused_split(&mut self, direction: FocusDirection) -> bool {
        let (axis, delta) = match direction {
            FocusDirection::Left => (SplitAxis::Horizontal, -0.04),
            FocusDirection::Right => (SplitAxis::Horizontal, 0.04),
            FocusDirection::Up => (SplitAxis::Vertical, -0.04),
            FocusDirection::Down => (SplitAxis::Vertical, 0.04),
        };
        self.root
            .as_mut()
            .is_some_and(|root| root.resize_nearest(self.focused_tile, axis, delta))
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
        ViewSpec {
            view_ref: format!("view:test/{name}"),
        }
    }

    fn view_set_with(n: usize) -> ViewSet {
        ViewSet::from_tiling(
            TilingSpec::default(),
            (0..n).map(|i| bound(&format!("v{i}"))).collect(),
        )
    }

    #[test]
    fn cross_view_set_move_retains_instance_local_state_and_all_subject_drafts() {
        use crate::ui::model::{InputBufferKey, RyeOsInputState};
        let mut source = view_set_with(1);
        let mut target = view_set_with(1);
        let tile = source.focused_tile;
        let instance = source.tiles[&tile].instance_key.clone();
        let key = InputBufferKey::new(instance.clone(), "view:test/v0", "message").storage_key();
        source.input_buffers.insert(
            key.clone(),
            RyeOsInputState {
                text: "unsent".into(),
                ..Default::default()
            },
        );
        source.field_query_editing = Some(instance.clone());
        assert!(source.move_tile_to_view_set(&mut target, tile));
        assert!(source.root.is_none());
        assert!(source.focus_target.is_none());
        assert!(source.input_buffers.is_empty());
        assert_eq!(target.focused_tile, tile);
        assert_eq!(target.tiles[&tile].instance_key, instance);
        assert_eq!(target.field_query_editing, Some(instance));
        assert_eq!(target.input_buffers[&key].text, "unsent");
        assert!(target.root.as_ref().unwrap().validate().is_ok());
    }

    #[test]
    fn cross_view_set_move_into_full_layout_is_atomic() {
        let mut source = view_set_with(1);
        let mut target = view_set_with(crate::layout::MAX_LAYOUT_TILES);
        let source_before = source.root.clone();
        let target_before = target.root.clone();
        let tile = source.focused_tile;
        assert!(!source.move_tile_to_view_set(&mut target, tile));
        assert_eq!(source.root, source_before);
        assert_eq!(target.root, target_before);
        assert!(source.tiles.contains_key(&tile));
        assert!(!target.tiles.contains_key(&tile));
    }

    #[test]
    fn initial_view_sets_and_later_opens_have_disjoint_instance_identities() {
        let mut first = view_set_with(128);
        let second = view_set_with(128);
        let second_ids = second.tile_ids();
        assert!(first.tile_ids().iter().all(|id| !second_ids.contains(id)));
        let added = first.add_tile(bound("later")).unwrap();
        assert!(!second_ids.contains(&added));
        assert_eq!(first.tiles.len(), 129);
        assert_eq!(first.root.as_ref().unwrap().validate(), Ok(()));
    }

    #[test]
    fn compute_layout_empty_center_has_no_tree() {
        assert_eq!(compute_layout(&TilingSpec::default(), &[]), None);
    }

    #[test]
    fn compute_layout_single_tile_is_monocle() {
        let tree = compute_layout(&TilingSpec::default(), &ids(&[7])).unwrap();
        assert_eq!(tree, LayoutTree::single(TileId::new(7)));
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
        assert_eq!(second.as_ref(), &LayoutTree::single(TileId::new(1)));
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
        assert_eq!(s1.as_ref(), &LayoutTree::single(TileId::new(2)));
        assert_eq!(s2.as_ref(), &LayoutTree::single(TileId::new(3)));
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
        assert_eq!(first.as_ref(), &LayoutTree::single(TileId::new(3)));
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
        assert_eq!(m1.as_ref(), &LayoutTree::single(TileId::new(1)));
        assert_eq!(m2.as_ref(), &LayoutTree::single(TileId::new(2)));
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
        assert_eq!(first.as_ref(), &LayoutTree::single(TileId::new(1)));
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
        let mut ws = view_set_with(2);
        let order_before = ws.tile_ids();
        let new_id = ws
            .add_tile(bound("new"))
            .expect("fixture layout accepts view");
        let order = ws.tile_ids();
        assert_eq!(order.len(), 3);
        assert_eq!(order[..2], order_before[..]);
        assert_eq!(*order.last().unwrap(), new_id, "insert: end appends");
        assert_eq!(ws.focused_tile, new_id, "new tile takes focus");
        assert!(matches!(
            ws.tiles.get(&new_id).map(|t| &t.local),
            Some(ViewLocalState::GenericList {
                cursor: 0,
                scroll: 0,
                ..
            })
        ));
        assert_eq!(
            ws.tiles.get(&new_id).map(|tile| tile.instance_key.as_str()),
            Some(format!("tile:{}", new_id.0).as_str())
        );
    }

    #[test]
    fn duplicate_view_refs_have_distinct_stable_instance_keys() {
        let mut ws = ViewSet::from_tiling(
            TilingSpec::default(),
            vec![bound("view:test/same"), bound("view:test/same")],
        );
        let ids = ws.tile_ids();
        let first = ws.tiles.get(&ids[0]).unwrap().instance_key.clone();
        let second = ws.tiles.get(&ids[1]).unwrap().instance_key.clone();
        assert_ne!(first, second);

        ws.focused_tile = ids[0];
        ws.replace_focused_view(bound("view:test/replaced"));
        assert_eq!(ws.tiles.get(&ids[0]).unwrap().instance_key, first);

        assert!(ws.close_tile(ids[1]));
        assert_eq!(ws.tiles.get(&ids[0]).unwrap().instance_key, first);
        assert!(!ws.tiles.values().any(|tile| tile.instance_key == second));
    }

    #[test]
    fn first_added_tile_takes_the_full_center() {
        let mut ws = ViewSet::from_tiling(TilingSpec::default(), Vec::new());
        assert!(ws.center_is_empty());
        assert!(ws.layout().is_none());
        let id = ws
            .add_tile(bound("solo"))
            .expect("fixture layout accepts view");
        assert!(!ws.center_is_empty());
        assert_eq!(ws.layout(), Some(LayoutTree::single(id)));
    }

    #[test]
    fn close_tile_keeps_order_and_refocuses_neighbor() {
        let mut ws = view_set_with(3);
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
    fn closing_last_tile_empties_center() {
        let mut ws = view_set_with(1);
        let only = ws.tile_ids()[0];
        assert!(ws.close_tile(only));
        assert!(ws.center_is_empty());
        assert!(ws.layout().is_none());
    }

    #[test]
    fn close_tile_ignores_unknown_tile() {
        let mut ws = view_set_with(3);
        assert!(!ws.close_tile(TileId::new(999)));
        assert_eq!(ws.tile_ids().len(), 3);
    }

    #[test]
    fn focus_next_cycles_center_order() {
        let mut ws = view_set_with(3);
        let order = ws.tile_ids();
        ws.focused_tile = order[0];
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
        let mut ws = view_set_with(3);
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
        let mut ws = view_set_with(3);
        let order = ws.tile_ids();
        assert!(ws.zoom_tile(order[2]));
        assert_eq!(ws.tile_ids(), vec![order[2], order[0], order[1]]);
        assert_eq!(ws.focused_tile, order[2]);
        // Zooming the leader swaps it with the runner-up.
        assert!(ws.zoom_tile(order[2]));
        assert_eq!(ws.tile_ids(), vec![order[0], order[2], order[1]]);
    }

    #[test]
    fn maximize_is_reversible_without_rewriting_the_layout() {
        let mut ws = view_set_with(3);
        let original = ws.layout().unwrap();
        let target = ws.tile_ids()[1];

        assert!(ws.toggle_maximized(target));
        assert_eq!(ws.layout(), Some(original.clone()));
        assert_eq!(ws.presentation_layout(), Some(LayoutTree::single(target)));
        assert_eq!(ws.focused_tile, target);

        assert!(ws.toggle_maximized(target));
        assert_eq!(ws.maximized_tile, None);
        assert_eq!(ws.presentation_layout(), Some(original));
    }

    #[test]
    fn resize_changes_geometry_not_the_authored_arrange_recipe() {
        let mut ws = view_set_with(2);
        let recipe = ws.tiling.clone();
        let before = ws.layout().unwrap();
        assert!(ws.resize_focused_split(FocusDirection::Left));
        assert_ne!(ws.layout().unwrap(), before);
        assert_eq!(ws.tiling, recipe);
        assert!(ws.resize_focused_split(FocusDirection::Right));
        assert_eq!(ws.layout().unwrap(), before);
        assert!(!ws.resize_focused_split(FocusDirection::Up));
    }

    #[test]
    fn focus_in_direction_uses_computed_geometry() {
        // Traversal is geometric: the left placement precedes the right.
        let mut ws = view_set_with(2);
        let order = ws.tile_ids();
        ws.focused_tile = order[0];
        assert!(ws.focus_in_direction(FocusDirection::Right));
        assert_eq!(ws.focused_tile, order[1]);
        assert!(ws.focus_in_direction(FocusDirection::Left));
        assert_eq!(ws.focused_tile, order[0]);
    }

    #[test]
    fn nested_move_retains_view_state_and_rejects_invalid_destination_atomically() {
        let mut ws = view_set_with(3);
        let ids = ws.tile_ids();
        let key = ws.tiles[&ids[0]].instance_key.clone();
        ws.tiles.get_mut(&ids[0]).unwrap().local = ViewLocalState::None;
        assert!(ws.move_tile_beside(ids[0], ids[2], FocusDirection::Down));
        assert_eq!(ws.tiles[&ids[0]].instance_key, key);
        assert_eq!(ws.tiles[&ids[0]].local, ViewLocalState::None);
        ws.root.as_ref().unwrap().validate().unwrap();
        let before = serde_json::to_value(&ws).unwrap();
        assert!(!ws.move_tile_beside(ids[0], TileId::new(999), FocusDirection::Up));
        assert!(!ws.move_tile_beside(ids[0], ids[0], FocusDirection::Up));
        assert_eq!(serde_json::to_value(&ws).unwrap(), before);
    }

    #[test]
    fn arranged_large_region_is_bounded_and_even() {
        let ws = view_set_with(crate::layout::MAX_LAYOUT_TILES);
        ws.root.as_ref().unwrap().validate().unwrap();
    }
}
