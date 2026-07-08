use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::dto::{
    RyeOsDimensionDto, RyeOsFileReadDto, RyeOsFileSpaceDto, RyeOsFilesDto, RyeOsItemsDto,
    RyeOsProjectsDto, RyeOsThreadsDto, RyeOsTopologyDto,
};
use super::effect::{RyeOsEffect, RyeOsEffectKind};
use super::scene_model::RyeOsSceneModel;
use super::view_model::{RyeOsMotionEventVm, RyeOsNoticeVm, RyeOsTone, RyeOsViewModel};
use crate::atlas::AtlasUiStateVm;
use crate::surface::{
    SlotContentSpec, SlotSpec, SlotsSpec, SurfaceSpec, SurfaceStyleSpec, builtin_default,
};
use crate::workspace::{ViewLocalState, ViewSpec, Workspace};
use std::collections::HashMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BrowserSession {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub surface_ref: String,
    #[serde(default)]
    pub user_principal_id: Option<String>,
    #[serde(default)]
    pub effective_surface: Option<serde_json::Value>,
    #[serde(default)]
    pub project_path: Option<String>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub granted_caps: Vec<String>,
    #[serde(default)]
    pub events_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BrowserViewport {
    pub width: u32,
    pub height: u32,
    pub device_pixel_ratio: f32,
}

impl Default for BrowserViewport {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            device_pixel_ratio: 1.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsEnvelope {
    pub schema_version: String,
    pub generation: u64,
    pub view_model: RyeOsViewModel,
    pub scene_model: RyeOsSceneModel,
    pub effects: Vec<RyeOsEffect>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RyeOsFilters {
    pub items_query: String,
    pub items_kind: String,
    pub services_query: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsFilesState {
    pub root: String,
    pub path: String,
}

impl Default for RyeOsFilesState {
    fn default() -> Self {
        Self {
            root: "project_ai".to_string(),
            path: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsNotice {
    pub id: String,
    pub message: String,
    pub tone: RyeOsTone,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RyeOsOverlayState {
    pub active: Option<String>,
    pub query: String,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RyeOsFocusTarget {
    WorkspaceTile { tile_id: String },
    Dock { edge: RyeOsDockEdge },
}

/// Buffer state only — ephemera, never braided. Where text LANDS is the
/// `input.route` facet on the seat braid (`ui::seat`), not a field
/// here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RyeOsInputState {
    pub text: String,
    pub cursor: usize,
    /// For a cyclable live filter: which declared field the buffer currently
    /// targets (index into `feeds.fields`). 0 for single-field inputs.
    #[serde(default)]
    pub filter_field: usize,
}

impl RyeOsInputState {
    pub fn insert_char(&mut self, ch: char) {
        let cursor = clamp_to_char_boundary(&self.text, self.cursor);
        self.text.insert(cursor, ch);
        self.cursor = cursor + ch.len_utf8();
    }

    pub fn delete_before_cursor(&mut self) {
        let cursor = clamp_to_char_boundary(&self.text, self.cursor);
        if cursor == 0 {
            return;
        }
        let prev = self.text[..cursor]
            .char_indices()
            .last()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.text.drain(prev..cursor);
        self.cursor = prev;
    }

    pub fn set_text(&mut self, text: String, cursor: usize) {
        self.text = text;
        self.cursor = clamp_to_char_boundary(&self.text, cursor);
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }
}

fn clamp_to_char_boundary(value: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(value.len());
    while cursor > 0 && !value.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

/// Layout-neutral key for a transient input buffer. The buffer belongs to
/// a view instance, not to a placement: the same `view:` rendered twice
/// (two tiles, a tile and a slot) has independent buffer state. The
/// `view_instance_id` is a layout address (`tile.<id>`, `slot.bottom`),
/// not a semantic category.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InputBufferKey {
    pub view_instance_id: String,
    pub view_ref: String,
    pub input_id: String,
}

impl InputBufferKey {
    pub fn new(
        view_instance_id: impl Into<String>,
        view_ref: impl Into<String>,
        input_id: impl Into<String>,
    ) -> Self {
        Self {
            view_instance_id: view_instance_id.into(),
            view_ref: view_ref.into(),
            input_id: input_id.into(),
        }
    }

    /// Stable string key for the buffer map (JSON map keys must be
    /// strings). The three components are NUL-joined so they never collide.
    pub fn storage_key(&self) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}",
            self.view_instance_id, self.view_ref, self.input_id
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsDockEdge {
    Top,
    Bottom,
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsDockSlotState {
    pub visible: bool,
    pub size: u16,
    pub content: RyeOsDockContent,
}

impl RyeOsDockSlotState {
    fn from_slot(slot: &SlotSpec) -> Self {
        Self {
            visible: slot.open,
            size: slot.size,
            content: match &slot.content {
                SlotContentSpec::View(view_ref) => RyeOsDockContent::View {
                    view_ref: view_ref.clone(),
                },
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RyeOsDockContent {
    /// A content-bound view in a slot — slots are view instances with an
    /// edge placement; the same bindings render in slots and tiles. Input
    /// is no longer a slot variant: it is a view that declares an `input`
    /// block (e.g. `view:ryeos/input` in the bottom slot).
    View { view_ref: String },
}

/// Edge slot state, initialized FROM the surface `slots` block. An
/// absent edge has no slot; a closed slot keeps its content but frees
/// its space.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsDockState {
    pub top: Option<RyeOsDockSlotState>,
    pub bottom: Option<RyeOsDockSlotState>,
    pub left: Option<RyeOsDockSlotState>,
    pub right: Option<RyeOsDockSlotState>,
}

impl Default for RyeOsDockState {
    fn default() -> Self {
        // The fallback surface's slots block is the only default source.
        Self::from_slots(&SlotsSpec::default())
    }
}

impl RyeOsDockState {
    pub fn from_slots(slots: &SlotsSpec) -> Self {
        Self {
            top: slots.top.as_ref().map(RyeOsDockSlotState::from_slot),
            bottom: slots.bottom.as_ref().map(RyeOsDockSlotState::from_slot),
            left: slots.left.as_ref().map(RyeOsDockSlotState::from_slot),
            right: slots.right.as_ref().map(RyeOsDockSlotState::from_slot),
        }
    }

    pub fn slot(&self, edge: RyeOsDockEdge) -> Option<&RyeOsDockSlotState> {
        match edge {
            RyeOsDockEdge::Top => self.top.as_ref(),
            RyeOsDockEdge::Bottom => self.bottom.as_ref(),
            RyeOsDockEdge::Left => self.left.as_ref(),
            RyeOsDockEdge::Right => self.right.as_ref(),
        }
    }

    pub fn slot_mut(&mut self, edge: RyeOsDockEdge) -> Option<&mut RyeOsDockSlotState> {
        match edge {
            RyeOsDockEdge::Top => self.top.as_mut(),
            RyeOsDockEdge::Bottom => self.bottom.as_mut(),
            RyeOsDockEdge::Left => self.left.as_mut(),
            RyeOsDockEdge::Right => self.right.as_mut(),
        }
    }

    /// Visible slots, paired with their edge and bound view ref.
    pub fn visible_slot_views(&self) -> Vec<(RyeOsDockEdge, String)> {
        [
            (RyeOsDockEdge::Top, &self.top),
            (RyeOsDockEdge::Bottom, &self.bottom),
            (RyeOsDockEdge::Left, &self.left),
            (RyeOsDockEdge::Right, &self.right),
        ]
        .into_iter()
        .filter_map(|(edge, slot)| slot.as_ref().map(|slot| (edge, slot)))
        .filter(|(_, slot)| slot.visible)
        .map(|(edge, slot)| match &slot.content {
            RyeOsDockContent::View { view_ref } => (edge, view_ref.clone()),
        })
        .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsUiState {
    pub filters: RyeOsFilters,
    pub files: RyeOsFilesState,
    #[serde(default)]
    pub overlay: RyeOsOverlayState,
    #[serde(default)]
    pub focus_target: Option<RyeOsFocusTarget>,
    /// Transient input buffers, keyed layout-neutrally by
    /// `InputBufferKey::storage_key()`. A buffer belongs to a view
    /// instance, not a placement; the same view rendered twice has
    /// independent buffers. Ephemera — never braided.
    #[serde(default)]
    pub input_buffers: BTreeMap<String, RyeOsInputState>,
    #[serde(default)]
    pub docks: RyeOsDockState,
    /// Ambient/backdrop atlas state — the empty-center `namespace_atlas`
    /// background. Surface-level, not a tile. Per-tile Atlas tiles keep
    /// their own arrangement in [`Self::tile_atlas`].
    #[serde(default)]
    pub atlas: AtlasUiStateVm,
    /// Per-tile atlas arrangements, keyed by tile id (string). Atlas/graph
    /// were never meant to show one fixed thing — a surface can host
    /// several independent atlas tiles, each with its own layers, lens,
    /// and projection. A tile with no entry falls back to [`Self::atlas`].
    #[serde(default)]
    pub tile_atlas: BTreeMap<String, AtlasUiStateVm>,
    pub motion: Vec<RyeOsMotionEventVm>,
    pub loading: BTreeMap<String, bool>,
    pub notices: Vec<RyeOsNotice>,
    pub route: Option<String>,
    #[serde(default)]
    pub top_status_visible: bool,
    #[serde(default = "default_true")]
    pub bottom_status_visible: bool,
    #[serde(default)]
    pub backdrop_break_amount: f32,
    #[serde(default)]
    pub backdrop_break_target: f32,
}

impl Default for RyeOsUiState {
    fn default() -> Self {
        Self {
            filters: RyeOsFilters::default(),
            files: RyeOsFilesState::default(),
            overlay: RyeOsOverlayState::default(),
            focus_target: None,
            input_buffers: BTreeMap::new(),
            docks: RyeOsDockState::default(),
            atlas: AtlasUiStateVm::default(),
            tile_atlas: BTreeMap::new(),
            motion: Vec::new(),
            loading: BTreeMap::new(),
            notices: Vec::new(),
            route: None,
            // Both status bars start hidden — their content was incoherent
            // and we have nothing settled to put there yet. Toggle-on still
            // works if we decide on content later.
            top_status_visible: false,
            bottom_status_visible: false,
            backdrop_break_amount: 0.0,
            backdrop_break_target: 0.0,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RyeOsDataState {
    pub session: Option<BrowserSession>,
    pub dimension: Option<RyeOsDimensionDto>,
    pub topology: Option<RyeOsTopologyDto>,
    pub projects: Option<RyeOsProjectsDto>,
    pub threads: Option<RyeOsThreadsDto>,
    pub items: Option<RyeOsItemsDto>,
    pub tile_items: HashMap<String, RyeOsItemsDto>,
    pub files: Option<RyeOsFilesDto>,
    pub file_space: Option<RyeOsFileSpaceDto>,
    pub tile_files: HashMap<String, RyeOsFilesDto>,
    /// Per-tile file-space, keyed by tile id, for atlas tiles whose
    /// `body.scope` declares a file-space root/path. Absent tile → the
    /// shared `file_space` (ambient / scopeless tiles).
    pub tile_file_space: HashMap<String, RyeOsFileSpaceDto>,
    pub file_read: Option<RyeOsFileReadDto>,
    /// Bound-view source responses, keyed by tile id (the generic data
    /// system: open JSON, projected through view bindings).
    #[serde(default)]
    pub sources: HashMap<String, serde_json::Value>,
    /// Transient projected timeline cache, keyed by the same source key as
    /// `sources`. Rebuilt only when source data lands so scroll keys do not
    /// re-project long transcripts on every frame.
    #[serde(default, skip)]
    pub(crate) timeline_sources: HashMap<String, RyeOsTimelineSourceCache>,
    /// The newest fetch effect id issued for each source key. A response only
    /// lands if it is that newest request (freshness guard): when a single-lens
    /// tile is reused for a new selection its source keys are stable, so an
    /// older in-flight fetch (the previous selection) resolving late must not
    /// overwrite the current one. Without this, a detail lens with several
    /// section fetches could render mixed data from two threads.
    #[serde(default)]
    pub source_epoch: HashMap<String, u64>,
    /// Transient live cognition stream for the tailed head thread —
    /// ephemeral deltas accumulated between durable snapshots. Not truth;
    /// the braid snapshot is. Cleared once a fresh snapshot supersedes it.
    #[serde(default)]
    pub live_delta: Option<RyeOsLiveDelta>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RyeOsTimelineSourceCache {
    pub entries: Vec<super::timeline::RyeOsTimelineEntryVm>,
    pub indents: Vec<u8>,
    pub sources: Vec<Option<super::timeline::TimelineEntrySource>>,
}

// The live cognition buffer type + its accumulation logic live with the rest
// of the thread-execution-stream concern in `super::timeline`.
pub use super::timeline::RyeOsLiveDelta;

/// An atlas tile's content scope is declared in its view `body.scope`
/// (content, not engine code): `{ kind, query }` narrow the AiSpace items.
/// Returns `(query, kind)`; either/both `None` when undeclared — such a
/// tile shares the global atlas dataset rather than fetching its own.
pub(crate) fn atlas_item_scope(
    binding: &super::content::ViewBinding,
) -> (Option<String>, Option<String>) {
    let scope = binding.body.get("scope");
    let field = |key: &str| {
        scope
            .and_then(|s| s.get(key))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    (field("query"), field("kind"))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsRuntimeState {
    pub viewport: BrowserViewport,
    pub now_ms: u64,
    #[serde(default)]
    pub last_tick_ms: u64,
    #[serde(default)]
    pub activity_pulse: f32,
    #[serde(default)]
    pub attention_until_ms: u64,
}

impl Default for RyeOsRuntimeState {
    fn default() -> Self {
        Self {
            viewport: BrowserViewport::default(),
            now_ms: 0,
            last_tick_ms: 0,
            activity_pulse: 0.0,
            attention_until_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RyeOsCore {
    pub data: RyeOsDataState,
    /// Resolved `view:` bindings embedded in the effective surface
    /// (views-as-content; every binding remains an addressable item).
    #[serde(default)]
    pub views: std::collections::BTreeMap<String, super::content::ViewBinding>,
    pub ui: RyeOsUiState,
    /// Seat braid (engine-local log while the engine holds append
    /// authority; see `ui::seat`). Seat truth — route, selection —
    /// folds from here, never from renderer state.
    #[serde(default)]
    pub seat: super::seat::SeatLog,
    /// Surface-declared chrome style (border treatment).
    #[serde(default)]
    pub style: SurfaceStyleSpec,
    pub workspace: Workspace,
    pub workspaces: Vec<Workspace>,
    pub active_workspace: usize,
    pub runtime: RyeOsRuntimeState,
    pub pending_effects: BTreeMap<u64, RyeOsEffectKind>,
    pub generation: u64,
    pub next_effect_id: u64,
}

impl RyeOsCore {
    pub fn new(session: BrowserSession, viewport: BrowserViewport, now_ms: u64) -> Self {
        let surface = session
            .effective_surface
            .as_ref()
            .and_then(|value| serde_json::from_value::<SurfaceSpec>(value.clone()).ok())
            .unwrap_or_else(builtin_default);
        let input_route = super::seat::InputRoute::from_surface_input(surface.input.as_ref());
        let mut core = Self::default();
        core.views = super::content::views_from_surface(session.effective_surface.as_ref());
        core.data.session = Some(session);
        core.runtime.viewport = viewport;
        core.runtime.now_ms = now_ms;
        core.runtime.last_tick_ms = now_ms;
        if let Some(route) = input_route {
            if let Ok(value) = serde_json::to_value(&route) {
                core.seat.append_facet(super::seat::KEY_INPUT_ROUTE, value);
            }
        }
        // Edge slots initialize FROM the surface slots block; the
        // fallback surface's slots are the only default source.
        core.ui.docks = RyeOsDockState::from_slots(&surface.slots);
        core.style = surface.style;
        core.workspace = surface.to_workspace();
        let blank = Workspace::from_tiling(surface.tiling.clone(), Vec::new());
        core.workspaces = vec![blank; 9];
        core.workspaces[0] = core.workspace.clone();
        core.active_workspace = 0;
        core
    }

    pub fn emit(&mut self, kind: RyeOsEffectKind) -> RyeOsEffect {
        self.next_effect_id += 1;
        self.pending_effects
            .insert(self.next_effect_id, kind.clone());
        RyeOsEffect {
            id: self.next_effect_id,
            kind,
        }
    }

    pub fn has_project_bound(&self) -> bool {
        self.data
            .session
            .as_ref()
            .and_then(|session| session.project_path.as_deref())
            .is_some_and(|path| !path.is_empty())
            || self
                .data
                .dimension
                .as_ref()
                .and_then(|dimension| dimension.project.as_ref())
                .is_some_and(|project| !project.path.is_empty())
    }

    pub fn initial_effects(&mut self) -> Vec<RyeOsEffect> {
        let needs_atlas = self.surface_uses_atlas_ambient();
        let mut needs_atlas_items = needs_atlas && self.ui.atlas.active_projection.is_ai_space();
        let needs_file_space = needs_atlas && self.ui.atlas.active_projection.is_file_space();
        // An empty center can host the ambient topology background; the
        // backdrop scene itself is client-side, but the atlas ambient
        // still wants topology when no tiles occupy the center.
        let mut needs_topology = self.workspace.center_is_empty();
        let mut bound_tiles: Vec<(crate::ids::TileId, String)> = Vec::new();
        // Per-tile scene-data fetches: an atlas tile whose `body.scope`
        // declares an item scope, or whose file-space projection has its own
        // arrangement, fetches its OWN data keyed to the tile — so two atlas
        // tiles can show genuinely different content, not just different
        // projections of one shared dataset. Scopeless tiles fall through to
        // the shared global fetch below (no regression).
        let mut tile_item_fetches: Vec<(String, Option<String>, Option<String>)> = Vec::new();
        let mut tile_file_fetches: Vec<(String, String, String)> = Vec::new();
        let has_project = self.has_project_bound();

        for tile_id in self.workspace.tile_ids() {
            let Some(tile) = self.workspace.tiles.get(&tile_id) else {
                continue;
            };
            let view_ref = tile.view.view_ref.clone();
            bound_tiles.push((tile_id, view_ref.clone()));
            // The scene widgets need engine data the generic source path
            // doesn't carry: graph wants topology, atlas wants topology plus
            // items or file space (per this tile's projection).
            match self
                .views
                .get(&view_ref)
                .map(|binding| binding.widget.as_str())
            {
                Some("atlas") => {
                    needs_topology = true;
                    match self.tile_atlas_state(tile_id).active_projection {
                        crate::atlas::AtlasProjectionVm::AiSpace => {
                            let scope = self
                                .views
                                .get(&view_ref)
                                .map(atlas_item_scope)
                                .unwrap_or_default();
                            if scope.0.is_some() || scope.1.is_some() {
                                tile_item_fetches.push((tile_id.0.to_string(), scope.0, scope.1));
                            } else {
                                needs_atlas_items = true;
                            }
                        }
                        crate::atlas::AtlasProjectionVm::FileSpace => {
                            if has_project {
                                let atlas = self.tile_atlas_state(tile_id);
                                tile_file_fetches.push((
                                    tile_id.0.to_string(),
                                    atlas.file_space_root.clone(),
                                    atlas.file_space_path.clone(),
                                ));
                            }
                        }
                    }
                }
                Some("graph") => needs_topology = true,
                _ => {}
            }
        }

        let mut effects = vec![
            self.emit(RyeOsEffectKind::FetchDimension),
            self.emit(RyeOsEffectKind::FetchProjects),
        ];
        for (tile_id, view_ref) in bound_tiles {
            effects.extend(self.emit_fetch_source(tile_id, &view_ref));
        }
        for (key, view_ref) in self.visible_dock_views() {
            effects.extend(self.emit_fetch_source_keyed(key, &view_ref));
        }
        // @-mention sources: fetch the refs each input declares, keyed so the
        // reader (key_context / CompleteInput) reads them back. A generic
        // FetchSource, so clients need no bespoke handling.
        let mention_fetches: Vec<(String, String)> = self
            .views
            .iter()
            .filter_map(|(view_ref, binding)| {
                let input = binding.input.as_ref()?;
                let mentions = input.mentions.as_ref()?;
                Some((
                    super::content::mention_source_key(view_ref, &input.id),
                    mentions.item_ref.clone(),
                ))
            })
            .collect();
        for (key, source_ref) in mention_fetches {
            effects.push(self.emit(RyeOsEffectKind::FetchSource {
                tile_id: key,
                source_ref,
                params: serde_json::json!({}),
            }));
        }
        // `completion` sources (the line-start `/` grammar): fetched through the
        // same generic keyed FetchSource as mentions, read back by the
        // slash-completion projectors. No bespoke commands effect.
        let completion_fetches: Vec<(String, String)> = self
            .views
            .iter()
            .filter_map(|(view_ref, binding)| {
                let input = binding.input.as_ref()?;
                let completion = input.completion.as_ref()?;
                Some((
                    super::content::completion_source_key(view_ref, &input.id),
                    completion.item_ref.clone(),
                ))
            })
            .collect();
        for (key, source_ref) in completion_fetches {
            effects.push(self.emit(RyeOsEffectKind::FetchSource {
                tile_id: key,
                source_ref,
                params: serde_json::json!({}),
            }));
        }
        for (tile_id, query, kind) in tile_item_fetches {
            effects.push(self.emit(RyeOsEffectKind::FetchItems {
                tile_id: Some(tile_id),
                query,
                kind,
                limit: 1000,
            }));
        }
        for (tile_id, root, path) in tile_file_fetches {
            effects.push(self.emit(RyeOsEffectKind::FetchFileSpace {
                tile_id: Some(tile_id),
                root,
                path,
                max_depth: 8,
                max_entries: 3000,
            }));
        }
        if needs_atlas_items {
            effects.push(self.emit(RyeOsEffectKind::FetchItems {
                tile_id: None,
                query: None,
                kind: None,
                limit: 1000,
            }));
        }
        if needs_file_space && self.has_project_bound() {
            effects.push(self.emit(RyeOsEffectKind::FetchFileSpace {
                tile_id: None,
                root: self.ui.atlas.file_space_root.clone(),
                path: self.ui.atlas.file_space_path.clone(),
                max_depth: 8,
                max_entries: 3000,
            }));
        }
        if needs_topology {
            effects.push(self.emit(RyeOsEffectKind::FetchTopology));
        }
        effects
    }

    /// Hint arrival: semantic hook for transient "look" notices. Visual pulse
    /// state is layered on this entry point; refetches are content-bound via
    /// `refresh.on_hint`.
    pub fn note_hint(&mut self, kind: &str, payload: &serde_json::Value) -> Vec<RyeOsEffect> {
        match kind {
            "thread" => {
                if payload
                    .get("event_type")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|event| {
                        matches!(
                            event,
                            "thread_failed" | "thread_killed" | "thread_timed_out"
                        )
                    })
                {
                    self.runtime.attention_until_ms = self.runtime.now_ms.saturating_add(3_000);
                }
            }
            _ => {}
        }
        self.effects_for_hint(kind)
    }

    pub(crate) fn bump_activity_pulse(&mut self, amount: f32) {
        self.runtime.activity_pulse = (self.runtime.activity_pulse + amount).min(1.0);
    }

    pub fn wants_fast_ticks(&self) -> bool {
        self.workspace.center_is_empty()
            || (self.surface_uses_backdrop_underlay() && self.workspace_has_transparent_view())
            || self.runtime.activity_pulse > 0.02
            || self.row_shimmer_active()
    }

    fn row_shimmer_active(&self) -> bool {
        let now_ms = self.runtime.now_ms;
        self.workspace.tiles.values().any(|tile| match &tile.local {
            ViewLocalState::GenericList { changed_rows, .. } => changed_rows
                .values()
                .any(|changed_at| now_ms.saturating_sub(*changed_at) < 1_200),
            ViewLocalState::None => false,
        })
    }

    /// Hint arrival: refetch every bound tile or visible slot whose binding
    /// declares `refresh.on_hint: <kind>` or includes it in an array. Content
    /// decides its own liveness.
    pub fn effects_for_hint(&mut self, kind: &str) -> Vec<RyeOsEffect> {
        let mut targets: Vec<(String, String)> = self
            .workspace
            .tile_ids()
            .into_iter()
            .filter_map(|tile_id| {
                let tile = self.workspace.tiles.get(&tile_id)?;
                let view_ref = &tile.view.view_ref;
                let binding = self.views.get(view_ref)?;
                refresh_matches_hint(binding.refresh.get("on_hint"), kind)
                    .then(|| (tile_id.0.to_string(), view_ref.clone()))
            })
            .collect();
        targets.extend(
            self.visible_dock_views()
                .into_iter()
                .filter(|(_, view_ref)| {
                    self.views.get(view_ref).is_some_and(|binding| {
                        refresh_matches_hint(binding.refresh.get("on_hint"), kind)
                    })
                }),
        );
        targets
            .into_iter()
            .flat_map(|(source_key, view_ref)| self.emit_fetch_source_keyed(source_key, &view_ref))
            .collect()
    }

    /// Emit the source fetch(es) for a bound view tile, resolving `@facet:`
    /// params against the seat fold (explicit references only). One effect for
    /// a single-source widget; one per section for a `sections` widget.
    pub fn emit_fetch_source(
        &mut self,
        tile_id: crate::ids::TileId,
        view_ref: &str,
    ) -> Vec<RyeOsEffect> {
        self.emit_fetch_source_keyed(tile_id.0.to_string(), view_ref)
    }

    /// Keyed variant: slots and other non-tile hosts subscribe with
    /// stable string keys (e.g. `dock:left`). The same key addresses the
    /// instance's transient input buffer: a view declaring `input.feeds`
    /// injects its buffer text into the named source param before fetch.
    ///
    /// A `sections` view fetches one source per section, each under its own
    /// `section_source_key` so the resolver reads them independently; sections
    /// carry no input buffer, so only the single-source path injects `feeds`.
    pub fn emit_fetch_source_keyed(
        &mut self,
        source_key: String,
        view_ref: &str,
    ) -> Vec<RyeOsEffect> {
        let Some(binding) = self.views.get(view_ref) else {
            return Vec::new();
        };
        if binding.widget == "sections" {
            let sections = binding.sections.clone();
            let fold = self.seat.fold();
            let resolved: Vec<(String, super::content::SourceBinding, serde_json::Value)> =
                sections
                    .iter()
                    .enumerate()
                    .map(|(index, section)| {
                        let params =
                            super::content::resolve_params(&section.source.params, |key| {
                                fold.get(key).cloned()
                            });
                        (
                            super::content::section_source_key(&source_key, index),
                            section.source.clone(),
                            params,
                        )
                    })
                    .collect();
            // Keep prior section responses while refetching. `source_epoch`
            // drops stale responses, and hint-driven activity refreshes would
            // otherwise blank a sections view every coalesced activity tick.
            return resolved
                .into_iter()
                .filter_map(|(key, source, params)| self.build_fetch_source(key, &source, params))
                .collect();
        }
        let Some(source) = binding.source.clone() else {
            return Vec::new();
        };
        let feeds = binding
            .input
            .as_ref()
            .and_then(|input| input.feeds.as_ref())
            .cloned();
        let input_id = binding.input.as_ref().map(|input| input.id.clone());
        let fold = self.seat.fold();
        let mut params =
            super::content::resolve_params(&source.params, |key| fold.get(key).cloned());
        // LIVE filter: the buffer writes ONE source param — the active field of a
        // cyclable filter, or the single declared param. Only the active field is
        // ever sent, so cycling never leaves a stale param behind.
        if let (Some(feeds), Some(input_id)) = (feeds, input_id) {
            let key = InputBufferKey::new(source_key.clone(), view_ref, input_id);
            let buffer = self.ui.input_buffers.get(&key.storage_key());
            let text = buffer.map(|b| b.text.clone()).unwrap_or_default();
            let field = buffer.map(|b| b.filter_field).unwrap_or(0);
            let param = feeds.active_param(field).to_string();
            if let Some(object) = params.as_object_mut() {
                object.insert(param, serde_json::Value::String(text));
            } else {
                params = serde_json::json!({ param: text });
            }
        }
        self.build_fetch_source(source_key, &source, params)
            .into_iter()
            .collect()
    }

    /// Emit one `FetchSource` for a resolved (key, source, params) triple, or
    /// skip when a param references an unset facet (e.g. the inspector's
    /// `@facet:selection.item` before anything is selected): that resolves to
    /// null — nothing to fetch — and dispatching the null arg the op rejects
    /// is a 500, not an empty view.
    fn build_fetch_source(
        &mut self,
        source_key: String,
        source: &super::content::SourceBinding,
        params: serde_json::Value,
    ) -> Option<RyeOsEffect> {
        if facet_param_unresolved(&source.params, &params) {
            return None;
        }
        let effect = self.emit(RyeOsEffectKind::FetchSource {
            tile_id: source_key.clone(),
            source_ref: source.item_ref.clone(),
            params,
        });
        // Mark this as the newest request for the key; an older in-flight fetch
        // for the same key that resolves later is then dropped on arrival.
        self.data.source_epoch.insert(source_key, effect.id);
        Some(effect)
    }

    /// Visible content-bound slot views, keyed for source fetches.
    pub fn visible_dock_views(&self) -> Vec<(String, String)> {
        self.ui
            .docks
            .visible_slot_views()
            .into_iter()
            .map(|(edge, view_ref)| (dock_source_key(edge), view_ref))
            .collect()
    }

    /// Does the instance addressed by `instance_id`/`view_ref` declare an
    /// input buffer? (Layout-neutral — slots and tiles answer the same.)
    pub fn instance_declares_input(&self, view_ref: &str) -> bool {
        self.views
            .get(view_ref)
            .is_some_and(|binding| binding.input.is_some())
    }

    fn surface_uses_atlas_ambient(&self) -> bool {
        self.data
            .session
            .as_ref()
            .and_then(|session| session.effective_surface.as_ref())
            .and_then(|value| serde_json::from_value::<SurfaceSpec>(value.clone()).ok())
            .and_then(|surface| surface.ambient)
            .is_some_and(|ambient| ambient.uses_namespace_atlas())
    }

    fn surface_uses_backdrop_underlay(&self) -> bool {
        self.data
            .session
            .as_ref()
            .and_then(|session| session.effective_surface.as_ref())
            .and_then(|value| serde_json::from_value::<SurfaceSpec>(value.clone()).ok())
            .is_some_and(|surface| {
                surface.backdrop.is_some()
                    && surface.ambient.is_some_and(|ambient| {
                        ambient.show_background.unwrap_or(true)
                            && ambient
                                .opacity
                                .is_some_and(|opacity| opacity > 0.0 && opacity < 1.0)
                    })
            })
    }

    fn workspace_has_transparent_view(&self) -> bool {
        self.workspace.tiles.values().any(|tile| {
            self.views
                .get(&tile.view.view_ref)
                .and_then(|binding| binding.presentation.background)
                .is_some_and(|background| {
                    matches!(
                        background,
                        super::content::ViewBackgroundPresentation::Transparent
                    )
                })
        })
    }

    pub fn bump_generation(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }

    /// Atlas arrangement for a specific tile, falling back to the ambient
    /// backdrop state when the tile has no per-tile entry yet.
    pub(crate) fn tile_atlas_state(&self, tile_id: crate::ids::TileId) -> &AtlasUiStateVm {
        self.ui
            .tile_atlas
            .get(&tile_id.0.to_string())
            .unwrap_or(&self.ui.atlas)
    }

    pub fn notice(&mut self, message: impl Into<String>, tone: RyeOsTone) {
        let id = format!("notice:{}", self.generation.saturating_add(1));
        self.ui.notices.push(RyeOsNotice {
            id,
            message: message.into(),
            tone,
        });
        const MAX_NOTICES: usize = 8;
        if self.ui.notices.len() > MAX_NOTICES {
            let excess = self.ui.notices.len() - MAX_NOTICES;
            self.ui.notices.drain(0..excess);
        }
        self.bump_generation();
    }

    /// Like `notice`, but skips when the most recent notice carries the same
    /// message — for repeatable actions (e.g. Tab on an untargetable route)
    /// that must surface the reason once without spamming it on every press.
    pub fn notice_deduped(&mut self, message: impl Into<String>, tone: RyeOsTone) {
        let message = message.into();
        if self
            .ui
            .notices
            .last()
            .is_some_and(|last| last.message == message)
        {
            return;
        }
        self.notice(message, tone);
    }

    pub(crate) fn rebuild_timeline_source_cache(&mut self, source_key: &str) {
        self.data.timeline_sources.remove(source_key);
        let Some(tile_id) = parse_source_tile_key(source_key) else {
            return;
        };
        let Some(tile) = self.workspace.tiles.get(&tile_id) else {
            return;
        };
        let Some(binding) = self.views.get(&tile.view.view_ref) else {
            return;
        };
        if binding.widget != "timeline" {
            return;
        }
        let Some(response) = self.data.sources.get(source_key) else {
            return;
        };
        let (mut entries, mut indents, mut sources) = super::timeline::timeline_entries_indented(
            super::content::project_records(binding, response),
        );
        if let Some(summary) = super::view_model::timeline_summary_entry(response) {
            entries.insert(0, summary);
            indents.insert(0, 0);
            sources.insert(0, None);
        }
        self.data.timeline_sources.insert(
            source_key.to_string(),
            RyeOsTimelineSourceCache {
                entries,
                indents,
                sources,
            },
        );
    }

    pub fn envelope(&self, effects: Vec<RyeOsEffect>) -> RyeOsEnvelope {
        RyeOsEnvelope {
            schema_version: "ryeos.ui.envelope.v1".to_string(),
            generation: self.generation,
            view_model: super::view_model::build_view_model(self),
            scene_model: super::scene_model::build_scene_model(self, &self.ui.atlas, None, None),
            effects,
        }
    }

    pub fn notices_vm(&self) -> Vec<RyeOsNoticeVm> {
        self.ui
            .notices
            .iter()
            .map(|notice| RyeOsNoticeVm {
                id: notice.id.clone(),
                message: notice.message.clone(),
                tone: notice.tone,
            })
            .collect()
    }

    /// The currently selected UI target. `None` means the workspace's
    /// focused tile remains selected.
    pub fn focus_target(&self) -> RyeOsFocusTarget {
        self.ui
            .focus_target
            .clone()
            .unwrap_or_else(|| RyeOsFocusTarget::WorkspaceTile {
                tile_id: self.workspace.focused_tile.0.to_string(),
            })
    }

    /// The view instance that currently owns input, if any. Input follows the
    /// selected UI target: a dock view, or a workspace tile when that tile
    /// declares `input`.
    pub fn focused_input_instance(&self) -> Option<(InputBufferKey, String)> {
        if let RyeOsFocusTarget::Dock { edge } = self.focus_target() {
            let view_ref = self
                .ui
                .docks
                .slot(edge)
                .filter(|slot| slot.visible)
                .map(|slot| match &slot.content {
                    RyeOsDockContent::View { view_ref } => view_ref.clone(),
                })?;
            if let Some(input) = self.views.get(&view_ref).and_then(|b| b.input.as_ref()) {
                return Some((
                    InputBufferKey::new(dock_source_key(edge), view_ref.clone(), input.id.clone()),
                    view_ref,
                ));
            }
            return None;
        }

        let focused = self.workspace.focused_tile;
        if let Some(ViewSpec { view_ref }) =
            self.workspace.tiles.get(&focused).map(|tile| &tile.view)
        {
            if let Some(input) = self.views.get(view_ref).and_then(|b| b.input.as_ref()) {
                return Some((
                    InputBufferKey::new(focused.0.to_string(), view_ref.clone(), input.id.clone()),
                    view_ref.clone(),
                ));
            }
        }
        None
    }

    pub fn default_input_edge(&self) -> Option<RyeOsDockEdge> {
        self.ordered_slot_views()
            .into_iter()
            .find_map(|(edge, view_ref)| {
                self.views
                    .get(&view_ref)
                    .and_then(|binding| binding.input.as_ref())
                    .map(|_| edge)
            })
    }

    fn ordered_slot_views(&self) -> Vec<(RyeOsDockEdge, String)> {
        let mut slots = self.ui.docks.visible_slot_views();
        // Bottom is the conventional initial input focus; sort it first.
        slots.sort_by_key(|(edge, _)| match edge {
            RyeOsDockEdge::Bottom => 0,
            RyeOsDockEdge::Left => 1,
            RyeOsDockEdge::Right => 2,
            RyeOsDockEdge::Top => 3,
        });
        slots
    }

    /// Whether any focused view instance owns input (printable keys edit a
    /// buffer rather than falling through to the keymap).
    pub fn has_focused_input(&self) -> bool {
        self.focused_input_instance().is_some()
    }

    /// Read-only access to the focused instance's input buffer.
    pub fn focused_input_buffer(&self) -> Option<&RyeOsInputState> {
        let (key, _) = self.focused_input_instance()?;
        self.ui.input_buffers.get(&key.storage_key())
    }

    /// Mutable access to the focused instance's input buffer, creating it
    /// on first edit.
    pub fn focused_input_buffer_mut(&mut self) -> Option<&mut RyeOsInputState> {
        let (key, _) = self.focused_input_instance()?;
        Some(self.ui.input_buffers.entry(key.storage_key()).or_default())
    }

    /// Resolve the focused-context capabilities the shared keymap needs.
    /// One base helper so terminal and web build the same context and don't
    /// drift — physical key mapping lives in `ryeos_key_command`, capability
    /// resolution lives here.
    pub fn key_context(&self) -> super::keymap::RyeOsKeyContext {
        let focused = self.focused_input_instance();
        let (text, cursor) = focused
            .as_ref()
            .and_then(|(key, _)| self.ui.input_buffers.get(&key.storage_key()))
            .map(|buf| (buf.text.clone(), buf.cursor))
            .unwrap_or_default();
        let input = focused
            .as_ref()
            .and_then(|(_, view_ref)| self.views.get(view_ref))
            .and_then(|binding| binding.input.as_ref());

        // Completion can accept now iff the focused input declares the
        // commands completion AND `accept_slash_completion` would produce a
        // result (cursor at end, leading single `/`, a matching record) —
        // the same predicate `CompleteInput` acts on, so Tab never dispatches
        // a no-op completion when it could cycle the target instead.
        let slash_can_accept = focused
            .as_ref()
            .and_then(|(key, view_ref)| {
                let completion = self
                    .views
                    .get(view_ref)?
                    .input
                    .as_ref()?
                    .completion
                    .as_ref()?;
                let response = self
                    .data
                    .sources
                    .get(&super::content::completion_source_key(
                        view_ref,
                        &key.input_id,
                    ))?;
                let records = super::content::completion_records(completion, response);
                super::tokenize::accept_slash_completion(records, &text, cursor).map(|_| ())
            })
            .is_some();

        // A mention can accept now iff the cursor sits in an `@`-token and the
        // declared mentions source has a matching ref — the same predicate
        // `CompleteInput` acts on, so Tab completes a mention rather than
        // cycling the target or no-op-ing.
        let mention_can_accept = focused
            .as_ref()
            .and_then(|(key, view_ref)| {
                let mentions = self
                    .views
                    .get(view_ref)?
                    .input
                    .as_ref()?
                    .mentions
                    .as_ref()?;
                let (_, partial) = super::tokenize::active_mention(&text, cursor)?;
                let response = self
                    .data
                    .sources
                    .get(&super::content::mention_source_key(view_ref, &key.input_id))?;
                let records = super::content::project_mentions(mentions, response);
                (!super::tokenize::mention_completion(&records, partial).is_empty()).then_some(())
            })
            .is_some();
        let input_can_accept_completion = slash_can_accept || mention_can_accept;
        let (focused_row_expandable, focused_row_expanded) =
            self.focused_row_expand_state().unwrap_or((false, false));

        super::keymap::RyeOsKeyContext {
            overlay_open: self.ui.overlay.active.is_some(),
            input_visible: focused.is_some() || self.default_input_edge().is_some(),
            input_focused: focused.is_some(),
            input_blurrable: matches!(self.focus_target(), RyeOsFocusTarget::Dock { .. }),
            input_has_text: !text.is_empty(),
            input_is_live_filter: input.is_some_and(|i| i.is_live_filter()),
            input_filter_fields: input
                .filter(|i| i.is_live_filter())
                .and_then(|i| i.feeds.as_ref())
                .is_some_and(|f| f.field_count() > 1),
            input_has_completion: input
                .is_some_and(|i| i.completion.is_some() || i.mentions.is_some()),
            input_can_accept_completion,
            // Targeting retargets the route, so it's only exposed for a
            // route-submit input (defense-in-depth; content validation also
            // degrades a target on a non-route input).
            input_target_cycle: input
                .filter(|i| i.submits_to_route())
                .and_then(|i| i.target.as_ref())
                .map(|t| t.cycle),
            // The head thread is mid-execution → esc interrupts it.
            head_thread_running: self
                .seat
                .fold()
                .input_route()
                .thread
                .as_deref()
                .is_some_and(|head| self.head_thread_running(head)),
            focused_row_expandable,
            focused_row_expanded,
        }
    }

    /// Whether the given thread is still executing per the fetched thread
    /// projections (a non-terminal status). Drives the feed's working
    /// indicator and the esc-interrupt binding.
    pub(crate) fn head_thread_running(&self, head: &str) -> bool {
        self.data.threads.as_ref().is_some_and(|threads| {
            threads.threads.iter().any(|row| {
                row.get("thread_id").and_then(serde_json::Value::as_str) == Some(head)
                    && row
                        .get("status")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|s| {
                            matches!(s, "running" | "created" | "accepted" | "pending")
                        })
            })
        })
    }

    pub(crate) fn note_source_row_changes(
        &mut self,
        source_key: &str,
        old: Option<&serde_json::Value>,
        new: &serde_json::Value,
    ) {
        let Some(tile_id) = parse_source_tile_key(source_key) else {
            return;
        };
        let Some(tile) = self.workspace.tiles.get(&tile_id) else {
            return;
        };
        let Some(binding) = self.views.get(&tile.view.view_ref) else {
            return;
        };
        let new_rows = projected_row_signatures(binding, new);
        if new_rows.is_empty() {
            return;
        }
        let old_rows = old
            .map(|value| projected_row_signatures(binding, value))
            .unwrap_or_default();
        let now_ms = self.runtime.now_ms;
        let Some(tile) = self.workspace.tiles.get_mut(&tile_id) else {
            return;
        };
        let ViewLocalState::GenericList { changed_rows, .. } = &mut tile.local else {
            return;
        };
        let mut live = std::collections::BTreeSet::new();
        let mut changed = false;
        for (key, signature) in new_rows {
            live.insert(key.clone());
            if old_rows.get(&key) != Some(&signature) {
                changed_rows.insert(key, now_ms);
                changed = true;
            }
        }
        changed_rows.retain(|key, changed_at| {
            live.contains(key) && now_ms.saturating_sub(*changed_at) <= 2_000
        });
        if changed {
            self.bump_activity_pulse(0.35);
        }
    }

    pub(crate) fn focused_row_expand_state(&self) -> Option<(bool, bool)> {
        let tile_id = self.workspace.focused_tile;
        let tile = self.workspace.tiles.get(&tile_id)?;
        let binding = self.views.get(&tile.view.view_ref)?;
        let fields = super::content::expand_fields(binding);
        if fields.is_empty() {
            return None;
        }
        let (key, _) = self.focused_row_key_and_record(tile_id, binding)?;
        let expanded = match &tile.local {
            ViewLocalState::GenericList { expanded_rows, .. } => expanded_rows.contains(&key),
            ViewLocalState::None => false,
        };
        Some((true, expanded))
    }

    pub(crate) fn set_focused_row_expanded(&mut self, expand: bool) -> bool {
        let tile_id = self.workspace.focused_tile;
        let Some(tile) = self.workspace.tiles.get(&tile_id) else {
            return false;
        };
        let Some(binding) = self.views.get(&tile.view.view_ref) else {
            return false;
        };
        if super::content::expand_fields(binding).is_empty() {
            return false;
        }
        let Some((key, _)) = self.focused_row_key_and_record(tile_id, binding) else {
            return false;
        };
        let Some(tile) = self.workspace.tiles.get_mut(&tile_id) else {
            return false;
        };
        let ViewLocalState::GenericList { expanded_rows, .. } = &mut tile.local else {
            return false;
        };
        if expand {
            expanded_rows.insert(key)
        } else {
            expanded_rows.remove(&key)
        }
    }

    fn focused_row_key_and_record(
        &self,
        tile_id: crate::ids::TileId,
        binding: &super::content::ViewBinding,
    ) -> Option<(String, serde_json::Value)> {
        let tile = self.workspace.tiles.get(&tile_id)?;
        let cursor = match &tile.local {
            ViewLocalState::GenericList { cursor, .. } => *cursor,
            ViewLocalState::None => 0,
        };
        let response = self.data.sources.get(&tile_id.0.to_string())?;
        match binding.widget.as_str() {
            "rows" | "table" => super::content::source_collection(binding, response)
                .get(cursor)
                .map(|record| (row_key(record, cursor), record.clone())),
            "timeline" => {
                let (mut entries, mut indents, mut sources) = self
                    .data
                    .timeline_sources
                    .get(&tile_id.0.to_string())
                    .map(|cache| {
                        (
                            cache.entries.clone(),
                            cache.indents.clone(),
                            cache.sources.clone(),
                        )
                    })
                    .unwrap_or_else(|| {
                        let (mut entries, mut indents, mut sources) =
                            super::timeline::timeline_entries_indented(
                                super::content::project_records(binding, response),
                            );
                        if let Some(summary) = super::view_model::timeline_summary_entry(response) {
                            entries.insert(0, summary);
                            indents.insert(0, 0);
                            sources.insert(0, None);
                        }
                        (entries, indents, sources)
                    });
                // `append_live_delta` can add non-expandable transient entries.
                super::timeline::append_live_delta(self, &mut entries);
                indents.resize(entries.len(), 0);
                sources.resize(entries.len(), None);
                let collapsed = match &tile.local {
                    ViewLocalState::GenericList { collapsed, .. } => collapsed,
                    ViewLocalState::None => return None,
                };
                let folded = super::timeline::fold_timeline(entries, indents, sources, collapsed);
                let selected = folded.entries.len().checked_sub(1).map(|last| {
                    let distance = cursor.min(last);
                    last - distance
                })?;
                folded
                    .sources
                    .get(selected)
                    .cloned()
                    .flatten()
                    .map(|source| (source.key, source.raw))
            }
            _ => None,
        }
    }
}

pub(crate) fn row_key(record: &serde_json::Value, index: usize) -> String {
    for field in ["id", "thread_id", "ref"] {
        if let Some(value) = super::content::field_path(record, field)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
        {
            return format!("{field}:{value}");
        }
    }
    format!("index:{index}")
}

fn projected_row_signatures(
    binding: &super::content::ViewBinding,
    response: &serde_json::Value,
) -> std::collections::BTreeMap<String, String> {
    match binding.widget.as_str() {
        "rows" => super::content::project_records(binding, response)
            .into_iter()
            .enumerate()
            .map(|(index, record)| {
                let signature = serde_json::json!({
                    "primary": record.primary,
                    "meta": record.meta,
                    "tone": record.tone,
                })
                .to_string();
                (row_key(&record.raw, index), signature)
            })
            .collect(),
        "table" => {
            let columns = super::content::table_columns(binding);
            super::content::project_table(binding, response, &columns)
                .into_iter()
                .enumerate()
                .map(|(index, record)| {
                    let signature = serde_json::json!({
                        "cells": record.cells,
                        "cell_tones": record.cell_tones,
                        "tone": record.tone,
                    })
                    .to_string();
                    (row_key(&record.raw, index), signature)
                })
                .collect()
        }
        _ => std::collections::BTreeMap::new(),
    }
}

fn parse_source_tile_key(source_key: &str) -> Option<crate::ids::TileId> {
    source_key.parse::<u64>().ok().map(crate::ids::TileId::new)
}

fn refresh_matches_hint(value: Option<&serde_json::Value>, kind: &str) -> bool {
    match value {
        Some(serde_json::Value::String(s)) => s == kind,
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .any(|item| item.as_str().is_some_and(|s| s == kind)),
        _ => false,
    }
}

/// True when a source param that was an `@facet:` reference resolved to
/// null — the facet isn't set, so the source has nothing to fetch.
fn facet_param_unresolved(original: &serde_json::Value, resolved: &serde_json::Value) -> bool {
    use serde_json::Value;
    match (original, resolved) {
        (Value::Object(orig), Value::Object(res)) => orig
            .iter()
            .any(|(k, ov)| facet_param_unresolved(ov, res.get(k).unwrap_or(&Value::Null))),
        (Value::String(s), rv) => s.starts_with("@facet:") && rv.is_null(),
        _ => false,
    }
}

/// Stable source-fetch key for a slot edge (also the buffer instance id).
pub fn dock_source_key(edge: RyeOsDockEdge) -> String {
    format!(
        "dock:{}",
        match edge {
            RyeOsDockEdge::Top => "top",
            RyeOsDockEdge::Bottom => "bottom",
            RyeOsDockEdge::Left => "left",
            RyeOsDockEdge::Right => "right",
        }
    )
}

impl Default for RyeOsCore {
    fn default() -> Self {
        let surface = builtin_default();
        let workspace = surface.to_workspace();
        let workspaces = vec![workspace.clone(); 9];
        Self {
            data: RyeOsDataState::default(),
            views: std::collections::BTreeMap::new(),
            ui: RyeOsUiState::default(),
            seat: super::seat::SeatLog::default(),
            style: surface.style,
            workspace,
            workspaces,
            active_workspace: 0,
            runtime: RyeOsRuntimeState::default(),
            pending_effects: BTreeMap::new(),
            generation: 0,
            next_effect_id: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ryeos_input_handles_non_boundary_cursor() {
        let mut input = RyeOsInputState {
            text: "é".to_string(),
            cursor: 1,
            ..Default::default()
        };
        input.insert_char('x');
        assert_eq!(input.text, "xé");
        assert_eq!(input.cursor, 1);

        input.set_text("aé".to_string(), 2);
        assert_eq!(input.cursor, 1);
        input.delete_before_cursor();
        assert_eq!(input.text, "é");
        assert_eq!(input.cursor, 0);
    }

    #[test]
    fn sections_view_emits_one_fetch_per_section_under_distinct_keys() {
        let session = BrowserSession {
            effective_surface: Some(serde_json::json!({
                "name": "t",
                "tiles": ["view:ryeos/ryeos/status"],
                "views": {
                    "view:ryeos/ryeos/status": {
                        "widget": "sections",
                        "sections": [
                            { "title": "Threads", "source": { "ref": "service:ui/ryeos-ui/threads/list", "collection": "rows" }, "projection": { "primary": "thread_id" } },
                            { "title": "Bundles", "source": { "ref": "service:ui/ryeos-ui/items/list", "collection": "rows" }, "projection": { "primary": "name" } }
                        ]
                    }
                }
            })),
            read_only: false,
            ..Default::default()
        };
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        let fetches: Vec<(String, String)> = core
            .initial_effects()
            .iter()
            .filter_map(|effect| match &effect.kind {
                RyeOsEffectKind::FetchSource {
                    tile_id,
                    source_ref,
                    ..
                } => Some((tile_id.clone(), source_ref.clone())),
                _ => None,
            })
            .collect();
        // One fetch per section, in section order, each addressing its own service.
        assert_eq!(fetches.len(), 2, "one fetch per section: {fetches:?}");
        assert_eq!(fetches[0].1, "service:ui/ryeos-ui/threads/list");
        assert_eq!(fetches[1].1, "service:ui/ryeos-ui/items/list");
        // Distinct per-section keys so the resolver reads each independently.
        assert!(fetches[0].0.ends_with("#section0"), "{fetches:?}");
        assert!(fetches[1].0.ends_with("#section1"), "{fetches:?}");
        assert_ne!(fetches[0].0, fetches[1].0);
    }

    #[test]
    fn bound_view_tiles_emit_generic_source_fetch() {
        let session = BrowserSession {
            effective_surface: Some(serde_json::json!({
                "name": "t",
                "tiles": ["view:ryeos/threads/list"],
                "views": {
                    "view:ryeos/threads/list": {
                        "widget": "rows",
                        "source": { "ref": "service:ui/ryeos-ui/threads/list", "params": { "limit": 5 }, "collection": "threads" },
                        "projections": { "primary": "item_ref" }
                    }
                }
            })),
            read_only: false,
            ..Default::default()
        };
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        let effects = core.initial_effects();
        let fetch = effects.iter().find_map(|effect| match &effect.kind {
            RyeOsEffectKind::FetchSource {
                source_ref, params, ..
            } => Some((source_ref.clone(), params.clone())),
            _ => None,
        });
        let (source_ref, params) = fetch.expect("bound tile emits FetchSource");
        assert_eq!(source_ref, "service:ui/ryeos-ui/threads/list");
        assert_eq!(params["limit"], 5);
    }

    #[test]
    fn hint_refetch_matches_string_and_list_forms() {
        let session = BrowserSession {
            effective_surface: Some(serde_json::json!({
                "name": "t",
                "tiles": ["view:ryeos/threads/list", "view:ryeos/node/status"],
                "views": {
                    "view:ryeos/threads/list": {
                        "widget": "rows",
                        "source": { "ref": "service:ui/ryeos-ui/threads/list", "collection": "threads" },
                        "projections": { "primary": "thread_id" },
                        "refresh": { "on_hint": ["thread", "activity"] }
                    },
                    "view:ryeos/node/status": {
                        "widget": "text",
                        "source": { "ref": "service:system/status" },
                        "refresh": { "on_hint": "thread" }
                    }
                }
            })),
            read_only: false,
            ..Default::default()
        };
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);

        let activity_fetches: Vec<String> = core
            .effects_for_hint("activity")
            .iter()
            .filter_map(|effect| match &effect.kind {
                RyeOsEffectKind::FetchSource { source_ref, .. } => Some(source_ref.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(activity_fetches, vec!["service:ui/ryeos-ui/threads/list"]);

        let thread_fetches: Vec<String> = core
            .effects_for_hint("thread")
            .iter()
            .filter_map(|effect| match &effect.kind {
                RyeOsEffectKind::FetchSource { source_ref, .. } => Some(source_ref.clone()),
                _ => None,
            })
            .collect();
        assert!(thread_fetches.contains(&"service:ui/ryeos-ui/threads/list".to_string()));
        assert!(thread_fetches.contains(&"service:system/status".to_string()));
    }

    #[test]
    fn hint_refetch_reaches_visible_slot_views() {
        let session = BrowserSession {
            effective_surface: Some(serde_json::json!({
                "name": "t",
                "tiles": [],
                "slots": {
                    "top": { "content": "view:ryeos/node/status", "open": true, "size": 1 }
                },
                "views": {
                    "view:ryeos/node/status": {
                        "widget": "text",
                        "source": { "ref": "service:system/status" },
                        "refresh": { "on_hint": "thread" }
                    }
                }
            })),
            read_only: false,
            ..Default::default()
        };
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        let fetches: Vec<(String, String)> = core
            .effects_for_hint("thread")
            .iter()
            .filter_map(|effect| match &effect.kind {
                RyeOsEffectKind::FetchSource {
                    tile_id,
                    source_ref,
                    ..
                } => Some((tile_id.clone(), source_ref.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            fetches,
            vec![("dock:top".to_string(), "service:system/status".to_string())]
        );
    }

    #[test]
    fn scoped_atlas_tiles_emit_independent_per_tile_item_fetches() {
        // Two atlas tiles, each a distinct view item declaring its own
        // `body.scope` — content-addressed scope. Each must fetch its OWN
        // items (tile-scoped), so the tiles can show different content sets.
        let session = BrowserSession {
            effective_surface: Some(serde_json::json!({
                "name": "t",
                "tiles": ["view:ryeos/atlas/knowledge", "view:ryeos/atlas/services"],
                "views": {
                    "view:ryeos/atlas/knowledge": { "widget": "atlas", "body": { "scope": { "kind": "knowledge" } } },
                    "view:ryeos/atlas/services": { "widget": "atlas", "body": { "scope": { "kind": "service" } } }
                }
            })),
            read_only: false,
            ..Default::default()
        };
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        let fetches: Vec<(Option<String>, Option<String>)> = core
            .initial_effects()
            .iter()
            .filter_map(|effect| match &effect.kind {
                RyeOsEffectKind::FetchItems { tile_id, kind, .. } => {
                    Some((tile_id.clone(), kind.clone()))
                }
                _ => None,
            })
            .collect();
        // One tile-scoped fetch per atlas tile, each carrying its kind; no
        // unscoped/global fetch (both tiles declare a scope).
        assert_eq!(fetches.len(), 2, "{fetches:?}");
        assert!(fetches.iter().all(|(tile_id, _)| tile_id.is_some()));
        let kinds: Vec<String> = fetches.iter().filter_map(|(_, k)| k.clone()).collect();
        assert!(kinds.contains(&"knowledge".to_string()), "{kinds:?}");
        assert!(kinds.contains(&"service".to_string()), "{kinds:?}");
    }

    #[test]
    fn scopeless_atlas_tile_falls_back_to_the_shared_item_fetch() {
        // An atlas tile with no declared scope shares the global dataset —
        // one unscoped (tile_id: None) fetch, no regression.
        let session = BrowserSession {
            effective_surface: Some(serde_json::json!({
                "name": "t",
                "tiles": ["view:ryeos/atlas"],
                "views": { "view:ryeos/atlas": { "widget": "atlas" } }
            })),
            read_only: false,
            ..Default::default()
        };
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        let fetches: Vec<Option<String>> = core
            .initial_effects()
            .iter()
            .filter_map(|effect| match &effect.kind {
                RyeOsEffectKind::FetchItems { tile_id, .. } => Some(tile_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(fetches, vec![None]);
    }

    #[test]
    fn default_docks_are_empty_no_views_named_in_code() {
        // The default slot set is empty: the engine never names product views.
        // Slots come only from surface data (the bundle YAMLs); a no-surface
        // default has no slots rather than fabricated input/threads/inspector.
        let docks = RyeOsDockState::default();
        assert!(docks.top.is_none());
        assert!(docks.bottom.is_none());
        assert!(docks.left.is_none());
        assert!(docks.right.is_none());
    }

    /// Seed a `view:ryeos/input` binding (chat box: route-fold submit).
    fn with_input_view(core: &mut RyeOsCore) {
        core.views.insert(
            "view:ryeos/input".to_string(),
            serde_json::from_value(serde_json::json!({
                "widget": "text",
                "input": { "id": "line", "placeholder": "Ask or run a command", "submit": "route" }
            }))
            .unwrap(),
        );
        // The default slot set is empty (no views named in code), so give the
        // core a bottom input slot the way a real surface's data would.
        core.ui.docks = RyeOsDockState::from_slots(&SlotsSpec {
            bottom: Some(SlotSpec {
                content: SlotContentSpec::View("view:ryeos/input".to_string()),
                open: true,
                size: 7,
            }),
            ..SlotsSpec::default()
        });
    }

    #[test]
    fn input_follows_focused_view_instance() {
        let mut core = RyeOsCore::default();
        // No input view embedded yet: no instance owns input.
        assert!(!core.has_focused_input());
        with_input_view(&mut core);
        // The visible bottom slot is available, but does not own input until
        // focus explicitly moves there.
        assert!(!core.has_focused_input());
        assert_eq!(core.default_input_edge(), Some(RyeOsDockEdge::Bottom));
        core.dispatch(super::super::event::RyeOsEvent::Ui {
            event: super::super::event::RyeOsUiEvent::FocusInput,
        });
        let (key, view_ref) = core.focused_input_instance().expect("input focused");
        assert_eq!(view_ref, "view:ryeos/input");
        assert_eq!(key.view_instance_id, "dock:bottom");
        assert_eq!(key.input_id, "line");
        // Hiding the slot removes the instance: focus falls through.
        core.ui.docks.bottom.as_mut().unwrap().visible = false;
        assert!(!core.has_focused_input());
    }

    #[test]
    fn ryeos_core_initializes_docks_from_surface_slots() {
        let session = BrowserSession {
            effective_surface: Some(serde_json::json!({
                "name": "custom-slots",
                "slots": {
                    "left": { "content": "view:custom/list", "open": true, "size": 20 },
                    "bottom": { "content": "view:ryeos/input", "open": false, "size": 5 }
                },
                "style": { "border": "thick" }
            })),
            ..Default::default()
        };
        let core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        let left = core.ui.docks.left.as_ref().expect("left slot");
        assert!(left.visible);
        assert_eq!(left.size, 20);
        assert!(matches!(
            &left.content,
            RyeOsDockContent::View { view_ref } if view_ref == "view:custom/list"
        ));
        let bottom = core.ui.docks.bottom.as_ref().expect("bottom slot");
        assert!(!bottom.visible);
        assert_eq!(bottom.size, 5);
        // Edges the surface does not declare have no slot.
        assert!(core.ui.docks.right.is_none());
        assert!(core.ui.docks.top.is_none());
        // Style flows from the surface, too.
        assert_eq!(core.style.border, crate::surface::BorderStyleSpec::Thick);
    }

    #[test]
    fn ryeos_core_vm_exposes_default_input_dock() {
        let mut core = RyeOsCore::default();
        with_input_view(&mut core);
        let vm = super::super::view_model::build_view_model(&core);
        assert!(vm.workspace.docks.bottom.is_some());
        assert!(vm.workspace.docks.top.is_none());
        assert!(vm.workspace.docks.left.is_none());
        assert!(vm.workspace.docks.right.is_none());
    }

    #[test]
    fn hidden_input_ignores_stale_input_events() {
        let mut core = RyeOsCore::default();
        with_input_view(&mut core);
        core.ui.docks.bottom.as_mut().unwrap().visible = false;

        let effects = core.dispatch(super::super::event::RyeOsEvent::Ui {
            event: super::super::event::RyeOsUiEvent::InsertInputChar { ch: 'x' },
        });
        assert!(effects.is_empty());
        assert!(core.focused_input_buffer().is_none());

        // No focused input instance: submit is a no-op.
        let effects = core.dispatch(super::super::event::RyeOsEvent::Ui {
            event: super::super::event::RyeOsUiEvent::SubmitInput,
        });
        assert!(effects.is_empty());
        assert!(core.ui.notices.is_empty());
    }

    #[test]
    fn visible_input_submit_respects_read_only_default() {
        let mut core = RyeOsCore::default();
        with_input_view(&mut core);
        core.focused_input_buffer_mut()
            .unwrap()
            .set_text("run this".to_string(), 8);
        let effects = core.dispatch(super::super::event::RyeOsEvent::Ui {
            event: super::super::event::RyeOsUiEvent::SubmitInput,
        });
        assert!(effects.is_empty());
        assert_eq!(core.ui.notices.len(), 1);
        assert_eq!(core.ui.notices[0].message, "This session is read-only.");
        assert_eq!(core.focused_input_buffer().unwrap().text, "run this");
    }
}
