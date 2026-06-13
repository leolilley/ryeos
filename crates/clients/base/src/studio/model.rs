use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::dto::{
    StudioDimensionDto, StudioFileReadDto, StudioFileSpaceDto, StudioFilesDto, StudioItemsDto,
    StudioProjectsDto, StudioThreadsDto, StudioTopologyDto,
};
use super::effect::{StudioEffect, StudioEffectKind};
use super::scene_model::StudioSceneModel;
use super::view_model::{StudioMotionEventVm, StudioNoticeVm, StudioTone, StudioViewModel};
use crate::atlas::AtlasUiStateVm;
use crate::surface::{
    builtin_default, SlotContentSpec, SlotSpec, SlotsSpec, SurfaceSpec, SurfaceStyleSpec,
};
use crate::workspace::{ViewSpec, Workspace};
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
pub struct StudioEnvelope {
    pub schema_version: String,
    pub generation: u64,
    pub view_model: StudioViewModel,
    pub scene_model: StudioSceneModel,
    pub effects: Vec<StudioEffect>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StudioFilters {
    pub items_query: String,
    pub items_kind: String,
    pub services_query: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioFilesState {
    pub root: String,
    pub path: String,
}

impl Default for StudioFilesState {
    fn default() -> Self {
        Self {
            root: "project_ai".to_string(),
            path: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioNotice {
    pub id: String,
    pub message: String,
    pub tone: StudioTone,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StudioLauncherState {
    pub open: bool,
    pub query: String,
    pub selected: usize,
}

/// Buffer state only — ephemera, never braided. Where text LANDS is the
/// `input.route` facet on the seat braid (`studio::seat`), not a field
/// here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StudioInputState {
    pub text: String,
    pub cursor: usize,
}

impl StudioInputState {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StudioDockEdge {
    Top,
    Bottom,
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioDockSlotState {
    pub visible: bool,
    pub size: u16,
    pub content: StudioDockContent,
}

impl StudioDockSlotState {
    fn from_slot(slot: &SlotSpec) -> Self {
        Self {
            visible: slot.open,
            size: slot.size,
            content: match &slot.content {
                SlotContentSpec::Input => StudioDockContent::Input,
                SlotContentSpec::View(view_ref) => StudioDockContent::View {
                    view_ref: view_ref.clone(),
                },
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StudioDockContent {
    /// The input chrome (engine widget, never content-targetable).
    Input,
    /// A content-bound view in a dock slot — docks are tiles with an
    /// edge; the same bindings render in both.
    View { view_ref: String },
}

/// Edge slot state, initialized FROM the surface `slots` block. An
/// absent edge has no slot; a closed slot keeps its content but frees
/// its space.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioDockState {
    pub top: Option<StudioDockSlotState>,
    pub bottom: Option<StudioDockSlotState>,
    pub left: Option<StudioDockSlotState>,
    pub right: Option<StudioDockSlotState>,
}

impl Default for StudioDockState {
    fn default() -> Self {
        // The fallback surface's slots block is the only default source.
        Self::from_slots(&SlotsSpec::default())
    }
}

impl StudioDockState {
    pub fn from_slots(slots: &SlotsSpec) -> Self {
        Self {
            top: slots.top.as_ref().map(StudioDockSlotState::from_slot),
            bottom: slots.bottom.as_ref().map(StudioDockSlotState::from_slot),
            left: slots.left.as_ref().map(StudioDockSlotState::from_slot),
            right: slots.right.as_ref().map(StudioDockSlotState::from_slot),
        }
    }

    pub fn slot(&self, edge: StudioDockEdge) -> Option<&StudioDockSlotState> {
        match edge {
            StudioDockEdge::Top => self.top.as_ref(),
            StudioDockEdge::Bottom => self.bottom.as_ref(),
            StudioDockEdge::Left => self.left.as_ref(),
            StudioDockEdge::Right => self.right.as_ref(),
        }
    }

    pub fn slot_mut(&mut self, edge: StudioDockEdge) -> Option<&mut StudioDockSlotState> {
        match edge {
            StudioDockEdge::Top => self.top.as_mut(),
            StudioDockEdge::Bottom => self.bottom.as_mut(),
            StudioDockEdge::Left => self.left.as_mut(),
            StudioDockEdge::Right => self.right.as_mut(),
        }
    }

    pub fn has_visible_input(&self) -> bool {
        [&self.top, &self.bottom, &self.left, &self.right]
            .iter()
            .filter_map(|slot| slot.as_ref())
            .any(|slot| slot.visible && matches!(slot.content, StudioDockContent::Input))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioUiState {
    pub filters: StudioFilters,
    pub files: StudioFilesState,
    pub launcher: StudioLauncherState,
    #[serde(default)]
    pub input: StudioInputState,
    #[serde(default)]
    pub docks: StudioDockState,
    #[serde(default)]
    pub atlas: AtlasUiStateVm,
    pub motion: Vec<StudioMotionEventVm>,
    pub loading: BTreeMap<String, bool>,
    pub notices: Vec<StudioNotice>,
    pub route: Option<String>,
    #[serde(default)]
    pub top_status_visible: bool,
    #[serde(default = "default_true")]
    pub bottom_status_visible: bool,
}

impl Default for StudioUiState {
    fn default() -> Self {
        Self {
            filters: StudioFilters::default(),
            files: StudioFilesState::default(),
            launcher: StudioLauncherState::default(),
            input: StudioInputState::default(),
            docks: StudioDockState::default(),
            atlas: AtlasUiStateVm::default(),
            motion: Vec::new(),
            loading: BTreeMap::new(),
            notices: Vec::new(),
            route: None,
            top_status_visible: false,
            bottom_status_visible: true,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StudioDataState {
    pub session: Option<BrowserSession>,
    pub dimension: Option<StudioDimensionDto>,
    pub topology: Option<StudioTopologyDto>,
    pub projects: Option<StudioProjectsDto>,
    pub threads: Option<StudioThreadsDto>,
    pub items: Option<StudioItemsDto>,
    pub tile_items: HashMap<String, StudioItemsDto>,
    pub files: Option<StudioFilesDto>,
    pub file_space: Option<StudioFileSpaceDto>,
    pub tile_files: HashMap<String, StudioFilesDto>,
    pub file_read: Option<StudioFileReadDto>,
    /// Command records from `service:commands/list` (completion data;
    /// open JSON — projected, never typed per-command).
    #[serde(default)]
    pub commands: Option<serde_json::Value>,
    /// Bound-view source responses, keyed by tile id (the generic data
    /// system: open JSON, projected through view bindings).
    #[serde(default)]
    pub sources: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioRuntimeState {
    pub viewport: BrowserViewport,
    pub now_ms: u64,
}

impl Default for StudioRuntimeState {
    fn default() -> Self {
        Self {
            viewport: BrowserViewport::default(),
            now_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StudioCore {
    pub data: StudioDataState,
    /// Resolved `view:` bindings embedded in the effective surface
    /// (views-as-content; every binding remains an addressable item).
    #[serde(default)]
    pub views: std::collections::BTreeMap<String, super::content::ViewBinding>,
    pub ui: StudioUiState,
    /// Seat braid (engine-local log while the engine holds append
    /// authority; see `studio::seat`). Seat truth — route, selection —
    /// folds from here, never from renderer state.
    #[serde(default)]
    pub seat: super::seat::SeatLog,
    /// Surface-declared chrome style (border treatment).
    #[serde(default)]
    pub style: SurfaceStyleSpec,
    pub workspace: Workspace,
    pub workspaces: Vec<Workspace>,
    pub active_workspace: usize,
    pub runtime: StudioRuntimeState,
    pub pending_effects: BTreeMap<u64, StudioEffectKind>,
    pub generation: u64,
    pub next_effect_id: u64,
}

impl StudioCore {
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
        if let Some(route) = input_route {
            if let Ok(value) = serde_json::to_value(&route) {
                core.seat.append_facet(super::seat::KEY_INPUT_ROUTE, value);
            }
        }
        // Edge slots initialize FROM the surface slots block; the
        // fallback surface's slots are the only default source.
        core.ui.docks = StudioDockState::from_slots(&surface.slots);
        core.style = surface.style;
        core.workspace = surface.to_workspace();
        let blank = Workspace::from_tiling(surface.tiling.clone(), Vec::new());
        core.workspaces = vec![blank; 9];
        core.workspaces[0] = core.workspace.clone();
        core.active_workspace = 0;
        core
    }

    pub fn emit(&mut self, kind: StudioEffectKind) -> StudioEffect {
        self.next_effect_id += 1;
        self.pending_effects
            .insert(self.next_effect_id, kind.clone());
        StudioEffect {
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

    pub fn studio_projects_service_available(&self) -> bool {
        self.data.dimension.as_ref().is_some_and(|dimension| {
            dimension
                .local_node
                .services
                .iter()
                .any(|service| service.service_ref == "service:ui/studio/projects/list")
        })
    }

    pub fn initial_effects(&mut self) -> Vec<StudioEffect> {
        let needs_atlas = self.surface_uses_atlas_ambient();
        let mut needs_atlas_items = needs_atlas && self.ui.atlas.active_projection.is_ai_space();
        let mut needs_file_space = needs_atlas && self.ui.atlas.active_projection.is_file_space();
        // Home (empty center) renders the ambient topology background.
        let mut needs_topology = self.workspace.is_home();
        let mut bound_tiles: Vec<(crate::ids::TileId, String)> = Vec::new();

        for tile_id in self.workspace.tile_ids() {
            let Some(tile) = self.workspace.tiles.get(&tile_id) else {
                continue;
            };
            match &tile.view {
                ViewSpec::Bound { view_ref } => {
                    bound_tiles.push((tile_id, view_ref.clone()));
                }
                ViewSpec::Atlas => {
                    match self.ui.atlas.active_projection {
                        crate::atlas::AtlasProjectionVm::AiSpace => needs_atlas_items = true,
                        crate::atlas::AtlasProjectionVm::FileSpace => needs_file_space = true,
                    }
                    needs_topology = true;
                }
                ViewSpec::Graph { .. } => needs_topology = true,
            }
        }

        let mut effects = vec![
            self.emit(StudioEffectKind::FetchDimension),
            self.emit(StudioEffectKind::FetchProjects),
            self.emit(StudioEffectKind::FetchCommands),
        ];
        for (tile_id, view_ref) in bound_tiles {
            if let Some(effect) = self.emit_fetch_source(tile_id, &view_ref) {
                effects.push(effect);
            }
        }
        for (key, view_ref) in self.visible_dock_views() {
            if let Some(effect) = self.emit_fetch_source_keyed(key, &view_ref) {
                effects.push(effect);
            }
        }
        if needs_atlas_items {
            effects.push(self.emit(StudioEffectKind::FetchItems {
                tile_id: None,
                query: None,
                kind: None,
                limit: 1000,
            }));
        }
        if needs_file_space && self.has_project_bound() {
            effects.push(self.emit(StudioEffectKind::FetchFileSpace {
                root: self.ui.atlas.file_space_root.clone(),
                path: self.ui.atlas.file_space_path.clone(),
                max_depth: 8,
                max_entries: 3000,
            }));
        }
        if needs_topology {
            effects.push(self.emit(StudioEffectKind::FetchTopology));
        }
        effects
    }

    /// Hint arrival: refetch every bound tile whose binding declares
    /// `refresh.on_hint: <kind>` (content decides its own liveness).
    pub fn effects_for_hint(&mut self, kind: &str) -> Vec<StudioEffect> {
        let targets: Vec<(crate::ids::TileId, String)> = self
            .workspace
            .tile_ids()
            .into_iter()
            .filter_map(|tile_id| {
                let tile = self.workspace.tiles.get(&tile_id)?;
                let ViewSpec::Bound { view_ref } = &tile.view else {
                    return None;
                };
                let binding = self.views.get(view_ref)?;
                (binding.refresh.get("on_hint").and_then(|v| v.as_str()) == Some(kind))
                    .then(|| (tile_id, view_ref.clone()))
            })
            .collect();
        targets
            .into_iter()
            .filter_map(|(tile_id, view_ref)| self.emit_fetch_source(tile_id, &view_ref))
            .collect()
    }

    /// Emit the generic source fetch for a bound view tile, resolving
    /// `@facet:` params against the seat fold (explicit references only).
    pub fn emit_fetch_source(
        &mut self,
        tile_id: crate::ids::TileId,
        view_ref: &str,
    ) -> Option<StudioEffect> {
        self.emit_fetch_source_keyed(tile_id.0.to_string(), view_ref)
    }

    /// Keyed variant: docks and other non-tile hosts subscribe with
    /// stable string keys (e.g. `dock:left`).
    pub fn emit_fetch_source_keyed(
        &mut self,
        source_key: String,
        view_ref: &str,
    ) -> Option<StudioEffect> {
        let binding = self.views.get(view_ref)?;
        let source = binding.source.clone()?;
        let fold = self.seat.fold();
        let params = super::content::resolve_params(&source.params, |key| fold.get(key).cloned());
        Some(self.emit(StudioEffectKind::FetchSource {
            tile_id: source_key,
            source_ref: source.item_ref,
            params,
        }))
    }

    /// Visible content-bound dock slots, keyed for source fetches.
    pub fn visible_dock_views(&self) -> Vec<(String, String)> {
        [
            ("dock:top", &self.ui.docks.top),
            ("dock:bottom", &self.ui.docks.bottom),
            ("dock:left", &self.ui.docks.left),
            ("dock:right", &self.ui.docks.right),
        ]
        .into_iter()
        .filter_map(|(key, slot)| slot.as_ref().map(|slot| (key, slot)))
        .filter(|(_, slot)| slot.visible)
        .filter_map(|(key, slot)| match &slot.content {
            StudioDockContent::View { view_ref } => Some((key.to_string(), view_ref.clone())),
            _ => None,
        })
        .collect()
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

    pub fn bump_generation(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }

    pub fn notice(&mut self, message: impl Into<String>, tone: StudioTone) {
        let id = format!("notice:{}", self.generation.saturating_add(1));
        self.ui.notices.push(StudioNotice {
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

    pub fn envelope(&self, effects: Vec<StudioEffect>) -> StudioEnvelope {
        StudioEnvelope {
            schema_version: "ryeos.studio.envelope.v1".to_string(),
            generation: self.generation,
            view_model: super::view_model::build_view_model(self),
            scene_model: super::scene_model::build_scene_model(self),
            effects,
        }
    }

    pub fn notices_vm(&self) -> Vec<StudioNoticeVm> {
        self.ui
            .notices
            .iter()
            .map(|notice| StudioNoticeVm {
                id: notice.id.clone(),
                message: notice.message.clone(),
                tone: notice.tone,
            })
            .collect()
    }
}

impl Default for StudioCore {
    fn default() -> Self {
        let surface = builtin_default();
        let workspace = surface.to_workspace();
        let workspaces = vec![workspace.clone(); 9];
        Self {
            data: StudioDataState::default(),
            views: std::collections::BTreeMap::new(),
            ui: StudioUiState::default(),
            seat: super::seat::SeatLog::default(),
            style: surface.style,
            workspace,
            workspaces,
            active_workspace: 0,
            runtime: StudioRuntimeState::default(),
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
    fn studio_input_handles_non_boundary_cursor() {
        let mut input = StudioInputState {
            text: "é".to_string(),
            cursor: 1,
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
    fn bound_view_tiles_emit_generic_source_fetch() {
        let session = BrowserSession {
            effective_surface: Some(serde_json::json!({
                "name": "t",
                "tiles": ["view:ryeos/threads/list"],
                "views": {
                    "view:ryeos/threads/list": {
                        "widget": "rows",
                        "source": { "ref": "service:ui/studio/threads", "params": { "limit": 5 }, "collection": "threads" },
                        "projections": { "primary": "item_ref" }
                    }
                }
            })),
            read_only: false,
            ..Default::default()
        };
        let mut core = StudioCore::new(session, BrowserViewport::default(), 0);
        let effects = core.initial_effects();
        let fetch = effects.iter().find_map(|effect| match &effect.kind {
            StudioEffectKind::FetchSource {
                source_ref, params, ..
            } => Some((source_ref.clone(), params.clone())),
            _ => None,
        });
        let (source_ref, params) = fetch.expect("bound tile emits FetchSource");
        assert_eq!(source_ref, "service:ui/studio/threads");
        assert_eq!(params["limit"], 5);
    }

    #[test]
    fn studio_dock_defaults_come_from_fallback_surface_slots() {
        let docks = StudioDockState::default();
        // The fallback surface declares no top slot at all.
        assert!(docks.top.is_none());
        let bottom = docks.bottom.as_ref().expect("bottom slot");
        assert!(bottom.visible);
        assert_eq!(bottom.size, 7);
        assert_eq!(bottom.content, StudioDockContent::Input);
        let left = docks.left.as_ref().expect("left slot");
        assert!(!left.visible);
        assert_eq!(left.size, 32);
        assert!(matches!(
            &left.content,
            StudioDockContent::View { view_ref } if view_ref == "view:ryeos/threads/list"
        ));
        let right = docks.right.as_ref().expect("right slot");
        assert!(!right.visible);
        assert_eq!(right.size, 40);
        assert!(matches!(
            &right.content,
            StudioDockContent::View { view_ref } if view_ref == "view:ryeos/item/inspector"
        ));
        assert!(docks.has_visible_input());
    }

    #[test]
    fn studio_docks_detect_input_on_any_visible_edge() {
        let mut docks = StudioDockState::default();
        docks.bottom.as_mut().unwrap().visible = false;
        assert!(!docks.has_visible_input());

        docks.top = Some(StudioDockSlotState {
            visible: true,
            size: 4,
            content: StudioDockContent::Input,
        });
        assert!(docks.has_visible_input());
    }

    #[test]
    fn studio_core_initializes_docks_from_surface_slots() {
        let session = BrowserSession {
            effective_surface: Some(serde_json::json!({
                "name": "custom-slots",
                "slots": {
                    "left": { "content": "view:custom/list", "open": true, "size": 20 },
                    "bottom": { "content": "input", "open": false, "size": 5 }
                },
                "style": { "border": "thick" }
            })),
            ..Default::default()
        };
        let core = StudioCore::new(session, BrowserViewport::default(), 0);
        let left = core.ui.docks.left.as_ref().expect("left slot");
        assert!(left.visible);
        assert_eq!(left.size, 20);
        assert!(matches!(
            &left.content,
            StudioDockContent::View { view_ref } if view_ref == "view:custom/list"
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
    fn studio_core_vm_exposes_default_input_dock() {
        let core = StudioCore::default();
        let vm = super::super::view_model::build_view_model(&core);
        assert!(vm.workspace.docks.bottom.is_some());
        assert!(vm.workspace.docks.top.is_none());
        assert!(vm.workspace.docks.left.is_none());
        assert!(vm.workspace.docks.right.is_none());
    }

    #[test]
    fn hidden_input_ignores_stale_input_events() {
        let mut core = StudioCore::default();
        core.ui.docks.bottom.as_mut().unwrap().visible = false;

        let effects = core.dispatch(super::super::event::StudioEvent::Ui {
            event: super::super::event::StudioUiEvent::InsertInputChar { ch: 'x' },
        });
        assert!(effects.is_empty());
        assert!(core.ui.input.text.is_empty());

        core.ui.input.set_text("run this".to_string(), 8);
        let effects = core.dispatch(super::super::event::StudioEvent::Ui {
            event: super::super::event::StudioUiEvent::SubmitInput,
        });
        assert!(effects.is_empty());
        assert!(core.ui.notices.is_empty());
    }

    #[test]
    fn visible_input_submit_respects_read_only_default() {
        let mut core = StudioCore::default();
        core.ui.input.set_text("run this".to_string(), 8);
        let effects = core.dispatch(super::super::event::StudioEvent::Ui {
            event: super::super::event::StudioUiEvent::SubmitInput,
        });
        assert!(effects.is_empty());
        assert_eq!(core.ui.notices.len(), 1);
        assert_eq!(core.ui.notices[0].message, "This session is read-only.");
        assert_eq!(core.ui.input.text, "run this");
    }
}
