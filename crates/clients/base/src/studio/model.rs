use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::dto::{
    StudioFileReadDto, StudioFilesDto, StudioGcStatusDto, StudioItemInspectionDto, StudioItemsDto,
    StudioProjectsDto, StudioSchedulesDto, StudioSnapshotDto, StudioThreadInspectionDto,
    StudioThreadsDto,
};
use super::effect::{StudioEffect, StudioEffectKind};
use super::scene_model::StudioSceneModel;
use super::view_model::{StudioNoticeVm, StudioTone, StudioViewModel};
use crate::surface::{LayoutNodeSpec, SurfaceLayoutSpec, SurfaceSpec, ViewKindSpec};
use crate::workspace::{ViewLocalState, ViewSpec, Workspace};
use std::collections::HashMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BrowserSession {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub surface_ref: String,
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
pub enum StudioInspectorState {
    Empty,
    Snapshot,
    Summary {
        title: String,
        detail: serde_json::Value,
    },
    Item {
        canonical_ref: String,
    },
    Thread {
        thread_id: String,
    },
    File {
        root: String,
        path: String,
    },
}

impl Default for StudioInspectorState {
    fn default() -> Self {
        Self::Empty
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StudioUiState {
    pub inspector: StudioInspectorState,
    pub filters: StudioFilters,
    pub files: StudioFilesState,
    pub launcher: StudioLauncherState,
    pub loading: BTreeMap<String, bool>,
    pub notices: Vec<StudioNotice>,
    pub route: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StudioDataState {
    pub session: Option<BrowserSession>,
    pub snapshot: Option<StudioSnapshotDto>,
    pub projects: Option<StudioProjectsDto>,
    pub threads: Option<StudioThreadsDto>,
    pub items: Option<StudioItemsDto>,
    pub tile_items: HashMap<String, StudioItemsDto>,
    pub schedules: Option<StudioSchedulesDto>,
    pub gc_status: Option<StudioGcStatusDto>,
    pub files: Option<StudioFilesDto>,
    pub tile_files: HashMap<String, StudioFilesDto>,
    pub file_read: Option<StudioFileReadDto>,
    pub item_inspection: Option<StudioItemInspectionDto>,
    pub thread_inspection: Option<StudioThreadInspectionDto>,
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
    pub ui: StudioUiState,
    pub workspace: Workspace,
    pub runtime: StudioRuntimeState,
    pub pending_effects: BTreeMap<u64, StudioEffectKind>,
    pub generation: u64,
    pub next_effect_id: u64,
}

impl StudioCore {
    pub fn new(session: BrowserSession, viewport: BrowserViewport, now_ms: u64) -> Self {
        let workspace = session
            .effective_surface
            .as_ref()
            .and_then(|value| serde_json::from_value::<SurfaceSpec>(value.clone()).ok())
            .map(|surface| surface.to_workspace())
            .unwrap_or_else(|| studio_default_surface().to_workspace());
        let mut core = Self::default();
        core.data.session = Some(session);
        core.runtime.viewport = viewport;
        core.runtime.now_ms = now_ms;
        core.ui.inspector = StudioInspectorState::Snapshot;
        core.workspace = workspace;
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

    pub fn initial_effects(&mut self) -> Vec<StudioEffect> {
        let mut needs_threads = false;
        let mut needs_schedules = false;
        let mut needs_gc = false;
        let mut item_tiles = Vec::new();
        let mut file_tiles = Vec::new();

        for tile_id in self.workspace.layout.tile_ids() {
            let Some(tile) = self.workspace.tiles.get(&tile_id) else {
                continue;
            };
            match tile.view {
                ViewSpec::Thread { .. } | ViewSpec::ThreadList => needs_threads = true,
                ViewSpec::SpaceBrowser { .. } => item_tiles.push((tile_id, tile.local.clone())),
                ViewSpec::Schedules => needs_schedules = true,
                ViewSpec::GcStatus => needs_gc = true,
                ViewSpec::Files => file_tiles.push((tile_id, tile.local.clone())),
                ViewSpec::Overview
                | ViewSpec::Remotes
                | ViewSpec::Services
                | ViewSpec::ItemInspector
                | ViewSpec::Projects
                | ViewSpec::Trust
                | ViewSpec::Graph { .. }
                | ViewSpec::EventInspector => {}
            }
        }

        let mut effects = vec![
            self.emit(StudioEffectKind::FetchSnapshot),
            self.emit(StudioEffectKind::FetchProjects),
        ];
        if needs_threads {
            effects.push(self.emit(StudioEffectKind::FetchThreads { limit: 200 }));
        }
        for (tile_id, local) in item_tiles {
            let (query, kind) = match local {
                ViewLocalState::SpaceBrowser { query, kind, .. } => (query, kind),
                _ => (String::new(), String::new()),
            };
            effects.push(self.emit(StudioEffectKind::FetchItems {
                tile_id: Some(tile_id.0.to_string()),
                query: non_empty(query),
                kind: non_empty(kind),
                limit: 1000,
            }));
        }
        if needs_schedules {
            effects.push(self.emit(StudioEffectKind::FetchSchedules));
        }
        if needs_gc {
            effects.push(self.emit(StudioEffectKind::FetchGcStatus));
        }
        for (tile_id, local) in file_tiles {
            let (root, path) = match local {
                ViewLocalState::Files { root, path, .. } => (root, path),
                _ => ("project_ai".to_string(), String::new()),
            };
            effects.push(self.emit(StudioEffectKind::ListFiles {
                tile_id: Some(tile_id.0.to_string()),
                root,
                path,
            }));
        }
        effects
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

fn non_empty(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn studio_default_surface() -> SurfaceSpec {
    let mut nodes = HashMap::new();
    nodes.insert(
        "root".to_string(),
        LayoutNodeSpec::Pane {
            view: ViewKindSpec::Graph,
        },
    );

    SurfaceSpec {
        name: "studio-base".to_string(),
        version: "1".to_string(),
        extends: None,
        description: Some("RyeOS Studio home space".to_string()),
        layout: SurfaceLayoutSpec {
            root: "root".to_string(),
            nodes,
        },
        input: None,
        ambient: None,
        affordances: Vec::new(),
        instruments: Vec::new(),
        capabilities: None,
    }
}

impl Default for StudioCore {
    fn default() -> Self {
        Self {
            data: StudioDataState::default(),
            ui: StudioUiState::default(),
            workspace: studio_default_surface().to_workspace(),
            runtime: StudioRuntimeState::default(),
            pending_effects: BTreeMap::new(),
            generation: 0,
            next_effect_id: 0,
        }
    }
}
