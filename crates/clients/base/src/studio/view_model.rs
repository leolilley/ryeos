use serde::{Deserialize, Serialize};

use super::event::StudioAction;
use super::model::{StudioCore, StudioInspectorState};
use super::scene_model::{build_scene_model, StudioSceneModel};
use crate::ids::TileId;
use crate::layout::{LayoutTree, SplitAxis};
use crate::surface::{AmbientAtlasStyleSpec, SurfaceSpec};
use crate::workspace::{TileState, ViewLocalState, ViewSpec};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioViewModel {
    pub schema_version: String,
    pub generation: u64,
    pub session: StudioSessionVm,
    pub chrome: StudioChromeVm,
    pub presentation: StudioPresentationVm,
    pub workspace: StudioWorkspaceVm,
    pub launcher: StudioLauncherVm,
    pub overlays: Vec<StudioOverlayVm>,
    pub notices: Vec<StudioNoticeVm>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StudioSessionVm {
    pub session_id: String,
    pub project_path: Option<String>,
    pub surface_ref: String,
    #[serde(default)]
    pub ambient: StudioAmbientVm,
    pub user_principal_id: Option<String>,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioAmbientVm {
    pub show_background: bool,
    pub opacity: Option<f32>,
    pub mode: StudioAmbientModeVm,
    pub atlas: Option<StudioAmbientAtlasVm>,
}

impl Default for StudioAmbientVm {
    fn default() -> Self {
        Self {
            show_background: true,
            opacity: None,
            mode: StudioAmbientModeVm::Ambient,
            atlas: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StudioAmbientModeVm {
    #[default]
    Ambient,
    NamespaceAtlas,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioAmbientAtlasVm {
    pub style: StudioAmbientAtlasStyleVm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StudioAmbientAtlasStyleVm {
    #[default]
    #[serde(rename = "flat_2d")]
    Flat2d,
    #[serde(rename = "paper_3d")]
    Paper3d,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioChromeVm {
    pub title: String,
    pub subtitle: String,
    pub health_label: String,
    pub health_tone: StudioTone,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioPresentationVm {
    pub schema_version: String,
    pub theme: StudioThemeVm,
    pub chrome: StudioPresentationChromeVm,
    pub metrics: StudioPresentationMetricsVm,
    pub frame: StudioFrameVm,
    pub home: StudioHomeVm,
    pub motion: Vec<StudioMotionEventVm>,
}

/// Shared semantic presentation signals.
///
/// Rust owns RyeOS meaning: counts, modes, health, focus, and semantic motion.
/// Renderers own pixels, easing, glyph choice, DOM/canvas/TUI implementation,
/// and how these signals are mapped into local visual affordances.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioPresentationMetricsVm {
    pub tile_count: usize,
    pub scene_object_count: usize,
    pub item_count: usize,
    pub thread_count: usize,
    pub project_count: usize,
    pub service_count: usize,
    pub schedule_count: usize,
    pub active_thread_count: i64,
    pub activity_level: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioThemeVm {
    pub id: String,
    pub tone: StudioTone,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioPresentationChromeVm {
    pub title: String,
    pub version_label: String,
    pub top_bar: StudioTopBarVm,
    pub status_bar: StudioStatusBarVm,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioTopBarVm {
    pub visible: bool,
    pub tabs: Vec<StudioWorkspaceTabVm>,
    pub focused_title: String,
    pub layout_symbol: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioWorkspaceTabVm {
    pub number: usize,
    pub active: bool,
    pub tile_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioStatusBarVm {
    pub visible: bool,
    pub segments: Vec<StudioStatusSegmentVm>,
    pub key_hint: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioStatusSegmentVm {
    pub id: String,
    pub label: Option<String>,
    pub value: String,
    pub tone: StudioTone,
    pub grow: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioFrameVm {
    pub mode: StudioFrameModeVm,
    pub corners: StudioFrameCornersVm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StudioFrameModeVm {
    Home,
    Workspace,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioFrameCornersVm {
    pub visible: bool,
    pub tone: StudioTone,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioHomeVm {
    pub brand: String,
    pub tagline: String,
    pub description: String,
    pub terminal_lines: Vec<String>,
    pub primary_label: String,
    pub secondary_label: String,
    pub secondary_url: String,
    pub install_command: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StudioMotionEventVm {
    TileEnter {
        tile_id: String,
    },
    TileExit {
        tile_id: String,
    },
    TileSplit {
        source_tile_id: String,
        new_tile_id: String,
        axis: StudioSplitAxisVm,
    },
    FocusChanged {
        tile_id: String,
    },
    HomeEnter,
    HomeExit,
    LauncherOpen,
    LauncherClose,
    TabChanged {
        workspace_number: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioWorkspaceVm {
    pub root: StudioLayoutNodeVm,
    pub focused_tile: String,
    pub is_home: bool,
    pub tile_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StudioLayoutNodeVm {
    Split {
        axis: StudioSplitAxisVm,
        ratio: f32,
        first: Box<StudioLayoutNodeVm>,
        second: Box<StudioLayoutNodeVm>,
    },
    Tile {
        tile_id: String,
        focused: bool,
        title: String,
        actions: Vec<StudioTileActionVm>,
        view: StudioViewVm,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StudioSplitAxisVm {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StudioViewVm {
    Overview {
        metrics: Vec<StudioMetricVm>,
        sections: Vec<StudioSectionVm>,
    },
    ThreadList {
        rows: Vec<StudioRowVm>,
    },
    Thread {
        thread_id: Option<String>,
        sections: Vec<StudioSectionVm>,
        code_blocks: Vec<StudioCodeBlockVm>,
    },
    Items {
        filters: StudioPanelFiltersVm,
        rows: Vec<StudioRowVm>,
    },
    Files {
        root: String,
        path: String,
        rows: Vec<StudioRowVm>,
        preview: Option<StudioFilePreviewVm>,
    },
    Rows {
        title: String,
        columns: Vec<String>,
        rows: Vec<StudioRowVm>,
    },
    Gc {
        running: bool,
        recent_events: Vec<serde_json::Value>,
    },
    Map {
        scene: StudioSceneModel,
    },
    Atlas {
        scene: StudioSceneModel,
    },
    Inspector(StudioInspectorVm),
    Placeholder {
        title: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioLauncherItemVm {
    pub label: String,
    pub hint: String,
    pub action: StudioAction,
    pub secondary_action: Option<StudioAction>,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioLauncherVm {
    pub open: bool,
    pub query: String,
    pub selected: usize,
    pub hint: String,
    pub items: Vec<StudioLauncherItemVm>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioTileActionVm {
    pub label: String,
    pub title: String,
    pub action: StudioAction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StudioOverlayVm {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioMetricVm {
    pub label: String,
    pub value: String,
    pub hint: Option<String>,
    pub tone: StudioTone,
    pub action: Option<StudioAction>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioSectionVm {
    pub title: String,
    pub rows: Vec<(String, String)>,
    pub action: Option<StudioAction>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioRowVm {
    pub id: String,
    pub primary: String,
    pub secondary: Option<String>,
    pub meta: Option<String>,
    pub kind: Option<String>,
    pub action: Option<StudioAction>,
    pub tone: StudioTone,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioFilePreviewVm {
    pub title: String,
    pub subtitle: String,
    pub kind: String,
    pub size: Option<usize>,
    pub truncated: bool,
    pub content: Option<String>,
    pub hint: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioPanelFiltersVm {
    pub tile_id: String,
    pub items_path: String,
    pub items_query: String,
    pub items_kind: String,
    pub item_kind_options: Vec<StudioFilterOptionVm>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioFilterOptionVm {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioInspectorVm {
    pub title: String,
    pub subtitle: Option<String>,
    pub sections: Vec<StudioSectionVm>,
    pub code_blocks: Vec<StudioCodeBlockVm>,
    pub empty: bool,
    pub empty_message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioCodeBlockVm {
    pub label: String,
    pub language: Option<String>,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioNoticeVm {
    pub id: String,
    pub message: String,
    pub tone: StudioTone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StudioTone {
    Good,
    Warn,
    Danger,
    #[default]
    Neutral,
    Accent,
}

pub fn build_view_model(core: &StudioCore) -> StudioViewModel {
    let session = session_vm(core);
    let health = health_label(core);
    let workspace = workspace_vm(core);
    let chrome = StudioChromeVm {
        title: "RyeOS".to_string(),
        subtitle: subtitle(core),
        health_label: health.clone(),
        health_tone: tone_for_health(&health),
    };
    StudioViewModel {
        schema_version: "ryeos.studio.vm.v1".to_string(),
        generation: core.generation,
        presentation: presentation_vm(core, &session, &chrome, &workspace),
        session,
        chrome,
        workspace,
        launcher: launcher(core),
        overlays: Vec::new(),
        notices: core.notices_vm(),
    }
}

fn presentation_vm(
    core: &StudioCore,
    session: &StudioSessionVm,
    chrome: &StudioChromeVm,
    workspace: &StudioWorkspaceVm,
) -> StudioPresentationVm {
    let version = ryeos_version(core);
    StudioPresentationVm {
        schema_version: "ryeos.studio.presentation.v1".to_string(),
        theme: StudioThemeVm {
            id: "gruvbox-optic".to_string(),
            tone: StudioTone::Accent,
        },
        chrome: StudioPresentationChromeVm {
            title: "Rye OS".to_string(),
            version_label: format!("RYE OS - {version}"),
            top_bar: top_bar_vm(core),
            status_bar: status_bar_vm(session, chrome, workspace, core, &version),
        },
        metrics: presentation_metrics_vm(core, workspace),
        frame: StudioFrameVm {
            mode: if workspace.is_home {
                StudioFrameModeVm::Home
            } else {
                StudioFrameModeVm::Workspace
            },
            corners: StudioFrameCornersVm {
                visible: true,
                tone: StudioTone::Accent,
            },
        },
        home: home_vm(),
        motion: core.ui.motion.clone(),
    }
}

fn top_bar_vm(core: &StudioCore) -> StudioTopBarVm {
    StudioTopBarVm {
        visible: core.ui.top_status_visible,
        tabs: core
            .workspaces
            .iter()
            .enumerate()
            .filter(|(index, workspace)| *index == core.active_workspace || !workspace.is_home())
            .map(|(index, workspace)| StudioWorkspaceTabVm {
                number: index + 1,
                active: index == core.active_workspace,
                tile_count: if index == core.active_workspace {
                    core.workspace.layout.tile_ids().len()
                } else {
                    workspace.layout.tile_ids().len()
                },
            })
            .collect(),
        focused_title: focused_tile_title(core),
        layout_symbol: layout_symbol(core),
    }
}

fn focused_tile_title(core: &StudioCore) -> String {
    core.workspace
        .tiles
        .get(&core.workspace.focused_tile)
        .map(|tile| tile.view.title())
        .unwrap_or_else(|| "home".to_string())
}

fn layout_symbol(core: &StudioCore) -> String {
    let master = core.workspace.master_tiles.len().max(1);
    let slave = core
        .workspace
        .layout
        .tile_ids()
        .len()
        .saturating_sub(master);
    format!("M{master}│S{slave}")
}

fn presentation_metrics_vm(
    core: &StudioCore,
    workspace: &StudioWorkspaceVm,
) -> StudioPresentationMetricsVm {
    let item_count = core
        .data
        .items
        .as_ref()
        .map(|items| items.items.len())
        .unwrap_or_default();
    let thread_count = core
        .data
        .threads
        .as_ref()
        .map(|threads| threads.threads.len())
        .unwrap_or_else(|| {
            core.data
                .dimension
                .as_ref()
                .map(|dimension| dimension.threads.active_count.max(0) as usize)
                .unwrap_or_default()
        });
    let project_count = core
        .data
        .projects
        .as_ref()
        .map(|projects| projects.projects.len())
        .unwrap_or_default();
    let service_count = core
        .data
        .dimension
        .as_ref()
        .map(|dimension| dimension.local_node.services.len())
        .unwrap_or_default();
    let schedule_count = core
        .data
        .schedules
        .as_ref()
        .map(|schedules| schedules.schedules.len())
        .or_else(|| {
            core.data
                .dimension
                .as_ref()
                .map(|dimension| dimension.schedules.total)
        })
        .unwrap_or_default();
    let active_thread_count = core
        .data
        .dimension
        .as_ref()
        .map(|dimension| dimension.threads.active_count)
        .unwrap_or_default();
    let scene_object_count = build_scene_model(core).objects.len();
    let activity_level = presentation_activity_level(
        workspace.tile_count,
        core.ui.motion.len(),
        core.ui.loading.len(),
        active_thread_count,
    );

    StudioPresentationMetricsVm {
        tile_count: workspace.tile_count,
        scene_object_count,
        item_count,
        thread_count,
        project_count,
        service_count,
        schedule_count,
        active_thread_count,
        activity_level,
    }
}

fn presentation_activity_level(
    tile_count: usize,
    motion_count: usize,
    loading_count: usize,
    active_thread_count: i64,
) -> f32 {
    let active_threads = active_thread_count.max(0) as f32;
    ((tile_count as f32 * 0.12)
        + (motion_count as f32 * 0.22)
        + (loading_count as f32 * 0.18)
        + (active_threads * 0.18))
        .clamp(0.0, 1.0)
}

fn status_bar_vm(
    session: &StudioSessionVm,
    chrome: &StudioChromeVm,
    workspace: &StudioWorkspaceVm,
    core: &StudioCore,
    version: &str,
) -> StudioStatusBarVm {
    let item_count = core
        .data
        .items
        .as_ref()
        .map(|items| items.items.len())
        .unwrap_or_default();
    let thread_count = core
        .data
        .threads
        .as_ref()
        .map(|threads| threads.threads.len())
        .unwrap_or_default();
    StudioStatusBarVm {
        visible: core.ui.bottom_status_visible,
        segments: vec![
            StudioStatusSegmentVm {
                id: "brand".to_string(),
                label: None,
                value: "rye os".to_string(),
                tone: StudioTone::Accent,
                grow: false,
            },
            StudioStatusSegmentVm {
                id: "version".to_string(),
                label: None,
                value: format!("v{version}"),
                tone: StudioTone::Neutral,
                grow: false,
            },
            StudioStatusSegmentVm {
                id: "health".to_string(),
                label: None,
                value: chrome.health_label.clone(),
                tone: chrome.health_tone,
                grow: false,
            },
            StudioStatusSegmentVm {
                id: "mode".to_string(),
                label: None,
                value: if session.read_only { "ro" } else { "rw" }.to_string(),
                tone: StudioTone::Neutral,
                grow: false,
            },
            StudioStatusSegmentVm {
                id: "tiles".to_string(),
                label: Some("tiles".to_string()),
                value: workspace.tile_count.to_string(),
                tone: StudioTone::Neutral,
                grow: false,
            },
            StudioStatusSegmentVm {
                id: "items".to_string(),
                label: Some("items".to_string()),
                value: item_count.to_string(),
                tone: StudioTone::Neutral,
                grow: false,
            },
            StudioStatusSegmentVm {
                id: "threads".to_string(),
                label: Some("threads".to_string()),
                value: thread_count.to_string(),
                tone: StudioTone::Neutral,
                grow: false,
            },
            StudioStatusSegmentVm {
                id: "principal".to_string(),
                label: Some("principal".to_string()),
                value: session
                    .user_principal_id
                    .as_deref()
                    .map(short_principal)
                    .unwrap_or_else(|| "local".to_string()),
                tone: StudioTone::Neutral,
                grow: false,
            },
            StudioStatusSegmentVm {
                id: "surface".to_string(),
                label: Some("surface".to_string()),
                value: short_surface_ref(&session.surface_ref),
                tone: StudioTone::Neutral,
                grow: false,
            },
            StudioStatusSegmentVm {
                id: "project".to_string(),
                label: None,
                value: session
                    .project_path
                    .clone()
                    .unwrap_or_else(|| "home".to_string()),
                tone: StudioTone::Neutral,
                grow: true,
            },
        ],
        key_hint: "alt+k open · alt+t/b bars · ctrl+←/→ tab · ctrl+↑/↓ move".to_string(),
    }
}

fn home_vm() -> StudioHomeVm {
    StudioHomeVm {
        brand: "RYE OS".to_string(),
        tagline: "portable operating system for ai".to_string(),
        description: "Persistent, signed AI substrate that travels with you across spaces, machines, and models.".to_string(),
        terminal_lines: vec![
            "hashes for truth. signatures for agency. attestations for proof.",
            "content-addressed. tamper-evident. verified by math.",
            "identity is a keypair. trust is a pin. authority is local.",
            "every item carries a chain of custody. every node verifies it.",
            "descriptors are trust pins, not credentials.",
            "wildcards rejected. capabilities attenuate. no escalation.",
            "the CAS is the commitment. the attestation is the proof.",
            "admission is proof of possession. not proof of account.",
            "two nodes, zero prior relationship, shared verified state.",
            "swap the model. swap the machine. the signatures hold.",
            "no central authority. no bearer tokens. no provider in the loop.",
            "closure complete, hashes verified, attestation signed.",
            "staged. mirrored. accepted. every byte accounted for.",
            "the hosting provider runs dns. the node runs authority.",
            "convergence without consensus. trust without coordination.",
        ]
        .into_iter()
        .map(str::to_string)
        .collect(),
        primary_label: "OPEN".to_string(),
        secondary_label: "GITHUB".to_string(),
        secondary_url: "https://github.com/leolilley/ryeos".to_string(),
        install_command: "pip install ryeos-mcp".to_string(),
    }
}

fn workspace_vm(core: &StudioCore) -> StudioWorkspaceVm {
    StudioWorkspaceVm {
        root: layout_node_vm(&core.workspace.layout, core),
        focused_tile: tile_id_text(core.workspace.focused_tile),
        is_home: core.workspace.is_home(),
        tile_count: core.workspace.layout.tile_ids().len(),
    }
}

fn layout_node_vm(node: &LayoutTree, core: &StudioCore) -> StudioLayoutNodeVm {
    match node {
        LayoutTree::Leaf(tile_id) => {
            let view = core
                .workspace
                .tiles
                .get(tile_id)
                .map(|tile| view_vm(core, *tile_id, tile))
                .unwrap_or_else(|| StudioViewVm::Placeholder {
                    title: "Missing view".to_string(),
                    message: format!("Tile {} is not present in the workspace.", tile_id.0),
                });
            let title = core
                .workspace
                .tiles
                .get(tile_id)
                .map(|tile| tile.view.title())
                .unwrap_or_else(|| "Missing".to_string());
            StudioLayoutNodeVm::Tile {
                tile_id: tile_id_text(*tile_id),
                focused: *tile_id == core.workspace.focused_tile,
                title,
                actions: tile_actions(core, *tile_id),
                view,
            }
        }
        LayoutTree::Split {
            axis,
            ratio,
            first,
            second,
        } => StudioLayoutNodeVm::Split {
            axis: match axis {
                SplitAxis::Horizontal => StudioSplitAxisVm::Horizontal,
                SplitAxis::Vertical => StudioSplitAxisVm::Vertical,
            },
            ratio: *ratio,
            first: Box::new(layout_node_vm(first, core)),
            second: Box::new(layout_node_vm(second, core)),
        },
    }
}

fn view_vm(core: &StudioCore, tile_id: TileId, tile: &TileState) -> StudioViewVm {
    match &tile.view {
        ViewSpec::Overview => overview(core),
        ViewSpec::ThreadList => StudioViewVm::ThreadList {
            rows: rows_for(core, &ViewSpec::ThreadList, Some(tile_id)),
        },
        ViewSpec::Thread { thread_id } => StudioViewVm::Thread {
            thread_id: thread_id.map(|id| id.0.to_string()),
            sections: Vec::new(),
            code_blocks: Vec::new(),
        },
        ViewSpec::SpaceBrowser { .. } => StudioViewVm::Items {
            filters: StudioPanelFiltersVm {
                tile_id: tile_id_text(tile_id),
                items_path: item_folder_state(tile),
                items_query: item_filter_state(tile).0,
                items_kind: item_filter_state(tile).1,
                item_kind_options: item_kind_options(),
            },
            rows: rows_for(core, &tile.view, Some(tile_id)),
        },
        ViewSpec::Files => files(core, tile_id, tile),
        ViewSpec::Services => StudioViewVm::Rows {
            title: "Services".to_string(),
            columns: vec!["Name".to_string(), "Detail".to_string()],
            rows: rows_for(core, &tile.view, None),
        },
        ViewSpec::Remotes => StudioViewVm::Rows {
            title: "Remotes".to_string(),
            columns: vec!["Name".to_string(), "Detail".to_string()],
            rows: rows_for(core, &tile.view, None),
        },
        ViewSpec::Schedules => StudioViewVm::Rows {
            title: "Schedules".to_string(),
            columns: vec!["Name".to_string(), "Detail".to_string()],
            rows: rows_for(core, &tile.view, None),
        },
        ViewSpec::GcStatus => StudioViewVm::Gc {
            running: core
                .data
                .gc_status
                .as_ref()
                .map(|gc| gc.running)
                .unwrap_or(false),
            recent_events: core
                .data
                .gc_status
                .as_ref()
                .map(|gc| gc.recent_events.clone())
                .unwrap_or_default(),
        },
        ViewSpec::Atlas => StudioViewVm::Atlas {
            scene: build_scene_model(core),
        },
        ViewSpec::Graph { .. } => StudioViewVm::Map {
            scene: build_scene_model(core),
        },
        ViewSpec::ItemInspector | ViewSpec::EventInspector => {
            StudioViewVm::Inspector(inspector(core))
        }
        ViewSpec::Projects => StudioViewVm::Rows {
            title: "Projects".to_string(),
            columns: vec!["Name".to_string(), "Root".to_string(), "Status".to_string()],
            rows: rows_for(core, &tile.view, Some(tile_id)),
        },
        ViewSpec::Trust => StudioViewVm::Rows {
            title: "Trust".to_string(),
            columns: vec![
                "Scope".to_string(),
                "Value".to_string(),
                "Detail".to_string(),
            ],
            rows: rows_for(core, &tile.view, None),
        },
    }
}

fn session_vm(core: &StudioCore) -> StudioSessionVm {
    let browser = core.data.session.as_ref();
    let dimension = core.data.dimension.as_ref();
    StudioSessionVm {
        session_id: browser
            .map(|session| session.session_id.clone())
            .or_else(|| dimension.map(|dimension| dimension.session.session_id.clone()))
            .unwrap_or_default(),
        project_path: browser
            .and_then(|session| session.project_path.clone())
            .or_else(|| {
                dimension.and_then(|dimension| dimension.project.as_ref().map(|p| p.path.clone()))
            }),
        surface_ref: browser
            .map(|session| session.surface_ref.clone())
            .or_else(|| dimension.map(|dimension| dimension.session.surface_ref.clone()))
            .unwrap_or_default(),
        ambient: ambient_vm(core),
        user_principal_id: browser
            .and_then(|session| session.user_principal_id.clone())
            .or_else(|| {
                dimension.and_then(|dimension| dimension.session.user_principal_id.clone())
            }),
        read_only: browser
            .map(|session| session.read_only)
            .or_else(|| dimension.map(|dimension| dimension.session.read_only))
            .unwrap_or(true),
    }
}

fn ambient_vm(core: &StudioCore) -> StudioAmbientVm {
    let Some(surface) = core
        .data
        .session
        .as_ref()
        .and_then(|session| session.effective_surface.as_ref())
        .and_then(|value| serde_json::from_value::<SurfaceSpec>(value.clone()).ok())
    else {
        return StudioAmbientVm {
            show_background: true,
            opacity: None,
            mode: StudioAmbientModeVm::Ambient,
            atlas: None,
        };
    };
    let Some(ambient) = surface.ambient else {
        return StudioAmbientVm {
            show_background: true,
            opacity: None,
            mode: StudioAmbientModeVm::Ambient,
            atlas: None,
        };
    };
    let atlas_style = ambient.namespace_atlas_style();
    StudioAmbientVm {
        show_background: ambient.show_background.unwrap_or(true),
        opacity: ambient.opacity,
        mode: if atlas_style.is_some() {
            StudioAmbientModeVm::NamespaceAtlas
        } else {
            StudioAmbientModeVm::Ambient
        },
        atlas: atlas_style.map(|style| StudioAmbientAtlasVm {
            style: match style {
                AmbientAtlasStyleSpec::Flat2d => StudioAmbientAtlasStyleVm::Flat2d,
                AmbientAtlasStyleSpec::Paper3d => StudioAmbientAtlasStyleVm::Paper3d,
            },
        }),
    }
}

pub(crate) fn launcher_items() -> Vec<StudioLauncherItemVm> {
    launcher_specs()
        .into_iter()
        .map(|(label, hint, view)| StudioLauncherItemVm {
            label: label.to_string(),
            hint: hint.to_string(),
            action: StudioAction::OpenView { view: view.clone() },
            secondary_action: Some(StudioAction::OpenNewView { view }),
            enabled: true,
        })
        .collect()
}

pub(crate) fn launcher_items_for(core: &StudioCore) -> Vec<StudioLauncherItemVm> {
    let mut items = context_launcher_items(core);
    items.extend(launcher_items());
    items
}

fn context_launcher_items(core: &StudioCore) -> Vec<StudioLauncherItemVm> {
    let mut items = Vec::new();

    if let Some(action) = inspect_action_for_focused_row(core) {
        items.push(StudioLauncherItemVm {
            label: "Inspect selection".to_string(),
            hint: focused_selection_hint(core).unwrap_or_else(|| "focused row".to_string()),
            action,
            secondary_action: None,
            enabled: true,
        });
    }

    if let Some((canonical_ref, label)) = focused_executable_item(core) {
        let parameters = serde_json::json!({});
        let pending = core.has_pending_invoke(&canonical_ref, &parameters);
        items.push(StudioLauncherItemVm {
            label: if pending {
                format!("Running {label}…")
            } else {
                format!("Run {label}")
            },
            hint: if pending {
                format!("pending · {canonical_ref}")
            } else {
                canonical_ref.clone()
            },
            action: StudioAction::ExecuteItem {
                item_ref: canonical_ref,
                parameters,
            },
            secondary_action: None,
            enabled: !session_vm(core).read_only && !pending,
        });
    }

    if let Some((thread_id, status)) = focused_cancellable_thread(core) {
        let pending = core.has_pending_cancel(&thread_id);
        items.push(StudioLauncherItemVm {
            label: if pending {
                format!("Cancelling {thread_id}…")
            } else {
                format!("Cancel {thread_id}")
            },
            hint: if pending {
                format!("pending · thread status: {status}")
            } else {
                format!("thread status: {status}")
            },
            action: StudioAction::CancelThread { thread_id },
            secondary_action: None,
            enabled: !session_vm(core).read_only && !pending,
        });
    }

    items
}

fn inspect_action_for_focused_row(core: &StudioCore) -> Option<StudioAction> {
    match action_for_focused_row(core)? {
        action @ (StudioAction::InspectItem { .. }
        | StudioAction::InspectThread { .. }
        | StudioAction::InspectSummary { .. }
        | StudioAction::ReadFile { .. }) => Some(action),
        _ => None,
    }
}

fn focused_executable_item(core: &StudioCore) -> Option<(String, String)> {
    let tile_id = core.workspace.focused_tile;
    let view = core.workspace.focused_view()?;
    if !matches!(view, ViewSpec::SpaceBrowser { .. }) {
        return None;
    }
    let cursor = selected_cursor(core, tile_id).unwrap_or(0);
    let row_ref = rows_for(core, view, Some(tile_id)).get(cursor)?.id.clone();
    let item = focused_item_set(core, tile_id)
        .into_iter()
        .find(|item| item.canonical_ref == row_ref && item.executable)?;
    let label = item_leaf_label(&item.label, &item.bare_id, &item.bare_id);
    Some((item.canonical_ref, label))
}

fn focused_item_set(core: &StudioCore, tile_id: TileId) -> Vec<super::dto::StudioItemDto> {
    let tile_id_text = tile_id_text(tile_id);
    core.data
        .tile_items
        .get(&tile_id_text)
        .or(core.data.items.as_ref())
        .map(|items| items.items.clone())
        .unwrap_or_default()
}

fn focused_cancellable_thread(core: &StudioCore) -> Option<(String, String)> {
    let tile_id = core.workspace.focused_tile;
    let view = core.workspace.focused_view()?;
    if !matches!(view, ViewSpec::ThreadList) {
        return None;
    }
    let cursor = selected_cursor(core, tile_id).unwrap_or(0);
    let thread = core.data.threads.as_ref()?.threads.get(cursor)?;
    let thread_id = field_text(thread, &["thread_id", "id"])?;
    let status = field_text(thread, &["status", "state"]).unwrap_or_else(|| "unknown".to_string());
    is_cancellable_thread_status(&status).then_some((thread_id, status))
}

fn is_cancellable_thread_status(status: &str) -> bool {
    matches!(status.to_ascii_lowercase().as_str(), "created" | "running")
}

fn focused_selection_hint(core: &StudioCore) -> Option<String> {
    let tile_id = core.workspace.focused_tile;
    let view = core.workspace.focused_view()?;
    let cursor = selected_cursor(core, tile_id).unwrap_or(0);
    let row = rows_for(core, view, Some(tile_id)).get(cursor)?.clone();
    Some(row.secondary.or(row.meta).unwrap_or(row.primary))
}

fn launcher_specs() -> [(&'static str, &'static str, ViewSpec); 11] {
    [
        (
            "Graph",
            "RyeOS topology",
            ViewSpec::Graph { graph_id: None },
        ),
        ("Atlas", "2D namespace map", ViewSpec::Atlas),
        (
            "Items",
            "RyeOS objects",
            ViewSpec::SpaceBrowser { project: None },
        ),
        ("Projects", "Known local roots", ViewSpec::Projects),
        ("Files", "Project files", ViewSpec::Files),
        ("Threads", "Runs and events", ViewSpec::ThreadList),
        ("Services", "Daemon endpoints", ViewSpec::Services),
        ("Remotes", "Federated nodes", ViewSpec::Remotes),
        ("Trust", "Principals and caps", ViewSpec::Trust),
        ("Schedules", "Timed work", ViewSpec::Schedules),
        ("GC", "State cleanup", ViewSpec::GcStatus),
    ]
}

fn short_principal(value: &str) -> String {
    if let Some(rest) = value.strip_prefix("fp:") {
        let prefix = rest.chars().take(8).collect::<String>();
        return format!("fp:{prefix}…");
    }
    truncate_middle(value, 14)
}

fn short_surface_ref(value: &str) -> String {
    value.strip_prefix("surface:").unwrap_or(value).to_string()
}

fn truncate_middle(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    let keep = max_chars.saturating_sub(1) / 2;
    let start = value.chars().take(keep).collect::<String>();
    let end = value
        .chars()
        .rev()
        .take(keep)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{start}…{end}")
}

fn launcher(core: &StudioCore) -> StudioLauncherVm {
    let query = core.ui.launcher.query.trim().to_lowercase();
    let items: Vec<_> = launcher_items_for(core)
        .into_iter()
        .filter(|item| {
            let haystack = format!("{} {}", item.label, item.hint).to_lowercase();
            query.is_empty() || haystack.contains(&query)
        })
        .collect();
    let selected = core.ui.launcher.selected.min(items.len().saturating_sub(1));
    StudioLauncherVm {
        open: core.ui.launcher.open,
        query: core.ui.launcher.query.clone(),
        selected,
        hint: "Alt+K open · Ctrl+←/→ tab · Ctrl+↑/↓ move · Ctrl+Shift+arrows resize · Alt+M master/slave · Alt+Q close"
            .to_string(),
        items,
    }
}

fn tile_actions(core: &StudioCore, tile_id: TileId) -> Vec<StudioTileActionVm> {
    let tile_id = tile_id_text(tile_id);
    let mut actions = vec![
        StudioTileActionVm {
            label: "↔".to_string(),
            title: "Split right".to_string(),
            action: StudioAction::SplitTile {
                tile_id: tile_id.clone(),
                axis: SplitAxis::Horizontal,
            },
        },
        StudioTileActionVm {
            label: "↕".to_string(),
            title: "Split down".to_string(),
            action: StudioAction::SplitTile {
                tile_id: tile_id.clone(),
                axis: SplitAxis::Vertical,
            },
        },
    ];
    if core.workspace.layout.tile_ids().len() > 1 {
        actions.push(StudioTileActionVm {
            label: "×".to_string(),
            title: "Close tile".to_string(),
            action: StudioAction::CloseTile { tile_id },
        });
    }
    actions
}

fn overview(core: &StudioCore) -> StudioViewVm {
    let health = health_label(core);
    StudioViewVm::Overview {
        metrics: vec![
            metric(
                "Health",
                &health,
                "Local node",
                tone_for_health(&health),
                None,
            ),
            metric(
                "Threads",
                &core
                    .data
                    .threads
                    .as_ref()
                    .map(|x| x.threads.len())
                    .unwrap_or(0)
                    .to_string(),
                "Recent runs",
                StudioTone::Neutral,
                Some(StudioAction::OpenView {
                    view: ViewSpec::ThreadList,
                }),
            ),
            metric(
                "Items",
                &core
                    .data
                    .items
                    .as_ref()
                    .map(|x| x.items.len())
                    .unwrap_or(0)
                    .to_string(),
                "Loaded objects",
                StudioTone::Neutral,
                Some(StudioAction::OpenView {
                    view: ViewSpec::SpaceBrowser { project: None },
                }),
            ),
            metric(
                "Projects",
                &core
                    .data
                    .projects
                    .as_ref()
                    .map(|x| x.projects.len())
                    .unwrap_or(0)
                    .to_string(),
                "Known local roots",
                StudioTone::Neutral,
                Some(StudioAction::OpenView {
                    view: ViewSpec::Projects,
                }),
            ),
            metric(
                "Services",
                &core
                    .data
                    .dimension
                    .as_ref()
                    .map(|x| x.local_node.services.len())
                    .unwrap_or(0)
                    .to_string(),
                "Daemon endpoints",
                StudioTone::Neutral,
                Some(StudioAction::OpenView {
                    view: ViewSpec::Services,
                }),
            ),
        ],
        sections: vec![StudioSectionVm {
            title: "Project".to_string(),
            rows: vec![
                (
                    "Path".to_string(),
                    session_vm(core)
                        .project_path
                        .unwrap_or_else(|| "No project bound".to_string()),
                ),
                ("Mode".to_string(), "RyeOS".to_string()),
            ],
            action: Some(StudioAction::SelectDimension),
        }],
    }
}

fn project_rows(core: &StudioCore) -> Vec<StudioRowVm> {
    let mut rows = Vec::new();
    let current_project = session_vm(core).project_path;
    if let Some(root) = current_project.as_ref().filter(|root| {
        !core.data.projects.as_ref().is_some_and(|projects| {
            projects
                .projects
                .iter()
                .any(|project| project.root == **root)
        })
    }) {
        rows.push(row(
            "__add_current_project".to_string(),
            "Register current project".to_string(),
            Some(root.clone()),
            Some("not registered".to_string()),
            Some(StudioAction::AddCurrentProject),
        ));
    }
    rows.extend(
        core.data
            .projects
            .as_ref()
            .map(|projects| projects.projects.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|project| {
                let local_id = project.local_id.clone();
                row(
                    local_id.clone(),
                    if project.name.is_empty() {
                        project.root.clone()
                    } else {
                        project.name
                    },
                    Some(project.root),
                    Some(
                        if project.exists {
                            "available"
                        } else {
                            "missing"
                        }
                        .to_string(),
                    ),
                    project
                        .exists
                        .then_some(StudioAction::OpenProject { local_id }),
                )
            }),
    );
    rows
}

fn trust_rows(core: &StudioCore) -> Vec<StudioRowVm> {
    let session = session_vm(core);
    let dimension = core.data.dimension.as_ref();
    let mut rows = vec![
        row(
            "session_principal".to_string(),
            "Session principal".to_string(),
            session.user_principal_id.clone(),
            Some(
                if session.read_only {
                    "read only"
                } else {
                    "writable"
                }
                .to_string(),
            ),
            Some(StudioAction::InspectSummary {
                title: "Session trust".to_string(),
                detail: serde_json::json!({
                    "session_id": session.session_id,
                    "surface_ref": session.surface_ref,
                    "user_principal_id": session.user_principal_id,
                    "read_only": session.read_only,
                }),
            }),
        ),
        row(
            "local_node_principal".to_string(),
            "Local node principal".to_string(),
            dimension.map(|dimension| dimension.local_node.identity.principal_id.clone()),
            dimension.map(|dimension| dimension.local_node.identity.fingerprint.clone()),
            dimension.map(|dimension| StudioAction::InspectSummary {
                title: "Local node identity".to_string(),
                detail: serde_json::to_value(&dimension.local_node.identity).unwrap_or_default(),
            }),
        ),
        row(
            "surface".to_string(),
            "Surface".to_string(),
            Some(session.surface_ref),
            Some("effective session surface".to_string()),
            None,
        ),
    ];

    let browser_caps = core
        .data
        .session
        .as_ref()
        .map(|session| session.granted_caps.clone())
        .unwrap_or_default();
    let granted_caps = dimension
        .map(|dimension| dimension.session.granted_caps.clone())
        .unwrap_or(browser_caps);
    rows.extend(granted_caps.into_iter().enumerate().map(|(index, cap)| {
        row(
            format!("granted_cap:{index}"),
            "Granted capability".to_string(),
            Some(cap.clone()),
            Some("session".to_string()),
            Some(StudioAction::InspectSummary {
                title: "Granted capability".to_string(),
                detail: serde_json::json!({ "capability": cap }),
            }),
        )
    }));

    if let Some(dimension) = dimension {
        rows.extend(dimension.local_node.services.iter().flat_map(|service| {
            service.required_caps.iter().map(move |cap| {
                row(
                    format!("required_cap:{}:{cap}", service.endpoint),
                    "Required capability".to_string(),
                    Some(cap.clone()),
                    Some(service.endpoint.clone()),
                    Some(StudioAction::InspectSummary {
                        title: service.endpoint.clone(),
                        detail: serde_json::to_value(service).unwrap_or_default(),
                    }),
                )
            })
        }));
    }

    rows
}

fn rows_for(core: &StudioCore, view: &ViewSpec, tile_id: Option<TileId>) -> Vec<StudioRowVm> {
    let mut rows: Vec<StudioRowVm> = match view {
        ViewSpec::ThreadList | ViewSpec::Thread { .. } => core
            .data
            .threads
            .as_ref()
            .map(|x| x.threads.clone())
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                let id =
                    field_text(&value, &["thread_id", "id"]).unwrap_or_else(|| index.to_string());
                row(
                    id.clone(),
                    id.clone(),
                    field_text(&value, &["item_ref", "item"]),
                    field_text(&value, &["status", "state"]),
                    Some(StudioAction::InspectThread { thread_id: id }),
                )
            })
            .collect(),
        ViewSpec::SpaceBrowser { .. } => {
            tile_id.map_or_else(Vec::new, |tile_id| item_rows(core, tile_id))
        }
        ViewSpec::Schedules => core
            .data
            .schedules
            .as_ref()
            .map(|x| x.schedules.clone())
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                row(
                    index.to_string(),
                    field_text(&value, &["schedule_id", "id", "name"])
                        .unwrap_or_else(|| "schedule".to_string()),
                    field_text(&value, &["item_ref", "target"]),
                    field_text(&value, &["enabled"]),
                    Some(StudioAction::InspectSummary {
                        title: "Schedule".to_string(),
                        detail: value,
                    }),
                )
            })
            .collect(),
        ViewSpec::Remotes => core
            .data
            .dimension
            .as_ref()
            .map(|x| x.remotes.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|remote| {
                row(
                    remote.name.clone(),
                    remote.name.clone(),
                    Some(remote.url.clone()),
                    Some(remote.principal_id.clone()),
                    Some(StudioAction::InspectSummary {
                        title: remote.name.clone(),
                        detail: serde_json::to_value(remote).unwrap_or_default(),
                    }),
                )
            })
            .collect(),
        ViewSpec::Services => core
            .data
            .dimension
            .as_ref()
            .map(|x| x.local_node.services.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|service| {
                row(
                    service.endpoint.clone(),
                    service.endpoint.clone(),
                    Some(service.service_ref.clone()),
                    Some(service.availability.clone()),
                    Some(StudioAction::InspectSummary {
                        title: service.endpoint.clone(),
                        detail: serde_json::to_value(service).unwrap_or_default(),
                    }),
                )
            })
            .collect(),
        ViewSpec::Projects => project_rows(core),
        ViewSpec::Trust => trust_rows(core),
        _ => Vec::new(),
    };
    if let Some(cursor) = tile_id.and_then(|tile_id| selected_cursor(core, tile_id)) {
        if let Some(row) = rows.get_mut(cursor) {
            row.selected = true;
        }
    }
    rows
}

fn files(core: &StudioCore, tile_id: TileId, tile: &TileState) -> StudioViewVm {
    let tile_id_text = tile_id_text(tile_id);
    let (root, path) = file_state(tile);
    StudioViewVm::Files {
        root: root.clone(),
        path: path.clone(),
        preview: file_preview_vm(core, tile, &root, &path),
        rows: mark_selected(
            core.data
                .tile_files
                .get(&tile_id_text)
                .or(core.data.files.as_ref())
                .as_ref()
                .map(|x| x.entries.clone())
                .unwrap_or_default()
                .into_iter()
                .map(|entry| {
                    let path = join_path(&path, &entry.name);
                    let kind = if entry.is_dir { "directory" } else { "file" };
                    let mut row = row(
                        path.clone(),
                        entry.name,
                        if entry.is_dir {
                            Some("directory".to_string())
                        } else {
                            None
                        },
                        entry.size.map(|size| size.to_string()),
                        Some(if entry.is_dir {
                            StudioAction::ListFiles {
                                tile_id: tile_id_text.clone(),
                                root: root.clone(),
                                path,
                            }
                        } else {
                            StudioAction::ReadFile {
                                root: root.clone(),
                                path,
                            }
                        }),
                    );
                    row.kind = Some(kind.to_string());
                    row
                })
                .collect(),
            selected_cursor(core, tile_id),
        ),
    }
}

fn file_preview_vm(
    core: &StudioCore,
    tile: &TileState,
    root: &str,
    path: &str,
) -> Option<StudioFilePreviewVm> {
    let files = core
        .data
        .tile_files
        .values()
        .find(|files| files.root == root && files.path == path)
        .or_else(|| {
            core.data
                .files
                .as_ref()
                .filter(|files| files.root == root && files.path == path)
        });
    let selected = files.and_then(|files| {
        let selected_index = match &tile.local {
            ViewLocalState::Files { cursor, .. } => *cursor,
            _ => 0,
        };
        files.entries.get(selected_index)
    });

    let Some(selected) = selected else {
        return Some(StudioFilePreviewVm {
            title: if path.is_empty() {
                "/".to_string()
            } else {
                path.to_string()
            },
            subtitle: root.to_string(),
            kind: "directory".to_string(),
            size: None,
            truncated: false,
            content: None,
            hint: "Select a file to preview or a directory to enter.".to_string(),
        });
    };

    let selected_path = join_path(path, &selected.name);
    if selected.is_dir {
        return Some(StudioFilePreviewVm {
            title: selected.name.clone(),
            subtitle: selected_path,
            kind: "directory".to_string(),
            size: None,
            truncated: false,
            content: None,
            hint: "Enter opens this directory.".to_string(),
        });
    }

    let read = core
        .data
        .file_read
        .as_ref()
        .filter(|file| file.root == root && file.path == selected_path);
    Some(StudioFilePreviewVm {
        title: selected.name.clone(),
        subtitle: selected_path,
        kind: "file".to_string(),
        size: read
            .map(|file| file.size)
            .or(selected.size.map(|size| size as usize)),
        truncated: read.is_some_and(|file| file.truncated),
        content: read.map(|file| file.content.clone()),
        hint: if read.is_some() {
            "Preview loaded from file read.".to_string()
        } else {
            "Enter reads this file into the preview.".to_string()
        },
    })
}

pub(crate) fn action_for_focused_row(core: &StudioCore) -> Option<StudioAction> {
    let tile_id = core.workspace.focused_tile;
    let view = core.workspace.focused_view()?;
    let cursor = selected_cursor(core, tile_id).unwrap_or(0);
    let rows = rows_for(core, view, Some(tile_id));
    rows.get(cursor).and_then(|row| row.action.clone())
}

fn selected_cursor(core: &StudioCore, tile_id: TileId) -> Option<usize> {
    let tile = core.workspace.tiles.get(&tile_id)?;
    match &tile.local {
        ViewLocalState::ThreadList { cursor, .. }
        | ViewLocalState::SpaceBrowser { cursor, .. }
        | ViewLocalState::Files { cursor, .. }
        | ViewLocalState::GenericList { cursor, .. } => Some(*cursor),
        ViewLocalState::Thread(state) => Some(state.timeline_cursor),
        ViewLocalState::None => None,
    }
}

fn mark_selected(mut rows: Vec<StudioRowVm>, cursor: Option<usize>) -> Vec<StudioRowVm> {
    if let Some(row) = cursor.and_then(|cursor| rows.get_mut(cursor)) {
        row.selected = true;
    }
    rows
}

fn item_filter_state(tile: &TileState) -> (String, String) {
    match &tile.local {
        ViewLocalState::SpaceBrowser { query, kind, .. } => (query.clone(), kind.clone()),
        _ => (String::new(), String::new()),
    }
}

fn item_folder_state(tile: &TileState) -> String {
    match &tile.local {
        ViewLocalState::SpaceBrowser { path, .. } => path.clone(),
        _ => String::new(),
    }
}

fn item_rows(core: &StudioCore, tile_id: TileId) -> Vec<StudioRowVm> {
    let tile_id_text = tile_id_text(tile_id);
    let folder = core
        .workspace
        .tiles
        .get(&tile_id)
        .map(item_folder_state)
        .unwrap_or_default();
    let folder_prefix = if folder.is_empty() {
        String::new()
    } else {
        format!("{folder}/")
    };
    let items = core
        .data
        .tile_items
        .get(&tile_id_text)
        .or(core.data.items.as_ref())
        .map(|x| x.items.clone())
        .unwrap_or_default();
    let mut dirs = BTreeSet::new();
    let mut rows = Vec::new();

    if !folder.is_empty() {
        let parent = folder
            .rsplit_once('/')
            .map(|(parent, _)| parent.to_string())
            .unwrap_or_default();
        let mut up = row(
            "..".to_string(),
            "..".to_string(),
            Some(parent.clone()),
            Some("folder".to_string()),
            Some(StudioAction::EnterItemFolder {
                tile_id: tile_id_text.clone(),
                path: parent,
            }),
        );
        up.kind = Some("folder_up".to_string());
        rows.push(up);
    }

    for item in items {
        let item_path = item_path(&item);
        let Some(remainder) = item_path.strip_prefix(&folder_prefix) else {
            continue;
        };
        if remainder.is_empty() {
            continue;
        }
        if let Some((dir, _)) = remainder.split_once('/') {
            dirs.insert(dir.to_string());
            continue;
        }
        let kind = item.item_kind.clone();
        let display = item_leaf_label(&item.label, &item.bare_id, remainder);
        let mut item_row = row(
            item.canonical_ref.clone(),
            display,
            Some(item.canonical_ref.clone()),
            Some(item.item_kind),
            Some(StudioAction::InspectItem {
                canonical_ref: item.canonical_ref,
            }),
        );
        item_row.kind = Some(kind);
        rows.push(item_row);
    }

    for dir in dirs.into_iter().rev() {
        let path = join_item_path(&folder, &dir);
        let mut dir_row = row(
            format!("folder:{path}"),
            dir,
            Some(path.clone()),
            Some("folder".to_string()),
            Some(StudioAction::EnterItemFolder {
                tile_id: tile_id_text.clone(),
                path,
            }),
        );
        dir_row.kind = Some("folder".to_string());
        rows.insert(if folder.is_empty() { 0 } else { 1 }, dir_row);
    }
    rows
}

fn item_path(item: &super::dto::StudioItemDto) -> String {
    item.namespace
        .as_ref()
        .filter(|namespace| !namespace.is_empty())
        .map(|namespace| {
            if item.bare_id.starts_with(&format!("{namespace}/")) {
                item.bare_id.clone()
            } else {
                join_item_path(namespace, &item.bare_id)
            }
        })
        .unwrap_or_else(|| item.bare_id.clone())
}

fn item_leaf_label(label: &str, bare_id: &str, fallback: &str) -> String {
    if !label.is_empty() && label != bare_id {
        label.to_string()
    } else {
        fallback.to_string()
    }
}

fn join_item_path(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_string()
    } else if child.is_empty() {
        parent.to_string()
    } else {
        format!("{parent}/{child}")
    }
}

fn file_state(tile: &TileState) -> (String, String) {
    match &tile.local {
        ViewLocalState::Files { root, path, .. } => (root.clone(), path.clone()),
        _ => ("project_ai".to_string(), String::new()),
    }
}

fn inspector(core: &StudioCore) -> StudioInspectorVm {
    match &core.ui.inspector {
        StudioInspectorState::Dimension => StudioInspectorVm {
            title: "RyeOS".to_string(),
            subtitle: Some("Current project and local node state".to_string()),
            sections: vec![StudioSectionVm {
                title: "Project".to_string(),
                rows: vec![
                    (
                        "Path".to_string(),
                        session_vm(core)
                            .project_path
                            .unwrap_or_else(|| "No project bound".to_string()),
                    ),
                    ("Mode".to_string(), "RyeOS".to_string()),
                    ("Health".to_string(), health_label(core)),
                ],
                action: None,
            }],
            code_blocks: Vec::new(),
            empty: false,
            empty_message: None,
        },
        StudioInspectorState::Summary { title, detail } => {
            code_or_empty(title, None, serde_json::to_string_pretty(detail).ok(), None)
        }
        StudioInspectorState::Item { canonical_ref } => item_inspector(core, canonical_ref),
        StudioInspectorState::Thread { thread_id } => thread_inspector(core, thread_id),
        StudioInspectorState::File { root, path } => code_or_empty(
            path,
            Some(root),
            core.data.file_read.as_ref().map(|x| x.content.clone()),
            Some("Loading file…"),
        ),
        StudioInspectorState::Empty => StudioInspectorVm {
            title: "RyeOS".to_string(),
            subtitle: Some("Select an object to inspect it.".to_string()),
            sections: Vec::new(),
            code_blocks: Vec::new(),
            empty: true,
            empty_message: Some("Select an object to inspect it.".to_string()),
        },
    }
}

fn item_inspector(core: &StudioCore, canonical_ref: &str) -> StudioInspectorVm {
    let inspection = core.data.item_inspection.as_ref();
    let mut vm = code_or_empty(
        canonical_ref,
        Some("Item"),
        inspection.and_then(|x| serde_json::to_string_pretty(x).ok()),
        Some("Loading item details…"),
    );
    let parameters = serde_json::json!({});
    let pending = core.has_pending_invoke(canonical_ref, &parameters);
    if inspection.is_some_and(|x| x.item.executable) && !session_vm(core).read_only && !pending {
        vm.sections.push(StudioSectionVm {
            title: "Run item".to_string(),
            rows: vec![
                ("Action".to_string(), "Run".to_string()),
                ("Target".to_string(), canonical_ref.to_string()),
            ],
            action: Some(StudioAction::ExecuteItem {
                item_ref: canonical_ref.to_string(),
                parameters,
            }),
        });
    } else if inspection.is_some_and(|x| x.item.executable) && pending {
        vm.sections.push(StudioSectionVm {
            title: "Run item".to_string(),
            rows: vec![
                ("Action".to_string(), "Running".to_string()),
                ("Target".to_string(), canonical_ref.to_string()),
            ],
            action: None,
        });
    }
    vm
}

fn thread_inspector(core: &StudioCore, thread_id: &str) -> StudioInspectorVm {
    let inspection = core.data.thread_inspection.as_ref();
    let mut vm = code_or_empty(
        thread_id,
        Some("Thread"),
        inspection.and_then(|x| serde_json::to_string_pretty(x).ok()),
        Some("Loading thread details…"),
    );
    if inspection
        .and_then(|x| field_text(&x.thread, &["status", "state"]))
        .is_some_and(|status| is_cancellable_thread_status(&status))
        && !session_vm(core).read_only
    {
        let pending = core.has_pending_cancel(thread_id);
        vm.sections.push(StudioSectionVm {
            title: "Cancel thread".to_string(),
            rows: vec![
                (
                    "Action".to_string(),
                    if pending { "Cancelling" } else { "Cancel" }.to_string(),
                ),
                ("Thread".to_string(), thread_id.to_string()),
            ],
            action: (!pending).then_some(StudioAction::CancelThread {
                thread_id: thread_id.to_string(),
            }),
        });
    }
    vm
}

fn code_or_empty(
    title: &str,
    subtitle: Option<&str>,
    content: Option<String>,
    empty_message: Option<&str>,
) -> StudioInspectorVm {
    let empty = content.is_none();
    StudioInspectorVm {
        title: title.to_string(),
        subtitle: subtitle.map(str::to_string),
        sections: Vec::new(),
        code_blocks: content
            .map(|content| {
                vec![StudioCodeBlockVm {
                    label: "Detail".to_string(),
                    language: Some("json".to_string()),
                    content,
                }]
            })
            .unwrap_or_default(),
        empty,
        empty_message: empty_message.map(str::to_string),
    }
}

fn metric(
    label: &str,
    value: &str,
    hint: &str,
    tone: StudioTone,
    action: Option<StudioAction>,
) -> StudioMetricVm {
    StudioMetricVm {
        label: label.to_string(),
        value: value.to_string(),
        hint: Some(hint.to_string()),
        tone,
        action,
    }
}

fn row(
    id: String,
    primary: String,
    secondary: Option<String>,
    meta: Option<String>,
    action: Option<StudioAction>,
) -> StudioRowVm {
    StudioRowVm {
        id,
        primary,
        secondary,
        meta,
        kind: None,
        action,
        tone: StudioTone::Neutral,
        selected: false,
    }
}

fn field_text(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| value.get(*key)).map(|v| {
        v.as_str()
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string())
    })
}

fn join_path(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_string()
    } else {
        format!("{base}/{name}")
    }
}

fn health_label(core: &StudioCore) -> String {
    core.data
        .dimension
        .as_ref()
        .and_then(|dimension| dimension.local_node.health.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or("connecting")
        .to_string()
}

fn ryeos_version(core: &StudioCore) -> String {
    core.data
        .dimension
        .as_ref()
        .and_then(|dimension| dimension.local_node.status.get("version"))
        .and_then(|v| v.as_str())
        .map(normalize_version_label)
        .unwrap_or_else(|| {
            option_env!("RYEOS_BUILD_VERSION")
                .unwrap_or(env!("CARGO_PKG_VERSION"))
                .to_string()
        })
}

fn normalize_version_label(version: &str) -> String {
    version.trim().trim_start_matches("ryeosd-").to_string()
}

fn tone_for_health(value: &str) -> StudioTone {
    let lower = value.to_ascii_lowercase();
    if lower.contains("healthy") {
        StudioTone::Good
    } else if lower.contains("degraded") {
        StudioTone::Warn
    } else if lower.contains("error") || lower.contains("failed") {
        StudioTone::Danger
    } else {
        StudioTone::Neutral
    }
}

fn subtitle(core: &StudioCore) -> String {
    session_vm(core)
        .project_path
        .unwrap_or_else(|| "Tiled RyeOS workspace".to_string())
}

fn item_kind_options() -> Vec<StudioFilterOptionVm> {
    [
        ("", "All kinds"),
        ("directive", "Directives"),
        ("tool", "Tools"),
        ("knowledge", "Knowledge"),
        ("config", "Config"),
    ]
    .into_iter()
    .map(|(value, label)| StudioFilterOptionVm {
        value: value.to_string(),
        label: label.to_string(),
    })
    .collect()
}

fn tile_id_text(id: TileId) -> String {
    id.0.to_string()
}
