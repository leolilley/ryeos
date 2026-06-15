use serde::{Deserialize, Serialize};

use super::content::{ProjectedRecord, TimelineRole};
use super::event::StudioAction;
use super::model::{StudioCore, StudioDockContent, StudioDockEdge, StudioDockSlotState};
use super::scene_model::{build_scene_model, StudioSceneModel};
use super::seat::InvokeTemplate;
use crate::ids::TileId;
use crate::layout::{LayoutTree, SplitAxis};
use crate::surface::{AmbientAtlasStyleSpec, SurfaceSpec};
use crate::workspace::{TileState, ViewLocalState, ViewSpec};

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
    /// Surface-declared border treatment for tiles, dock tiles, and
    /// panels: thick | thin | hidden | none. Renderers map the name to
    /// local glyphs/pixels — content declares, renderers map.
    pub border: String,
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
    pub corners: StudioFrameCornersVm,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioFrameCornersVm {
    pub visible: bool,
    pub tone: StudioTone,
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
    LauncherOpen,
    LauncherClose,
    TabChanged {
        workspace_number: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioWorkspaceVm {
    /// The computed layout tree. None when the center is empty (the
    /// backdrop scene shows) — the engine computes this from the ordered
    /// tile list and the tiling algorithm; it is never authored.
    pub root: Option<StudioLayoutNodeVm>,
    pub focused_tile: String,
    /// True when the center has no tiles. Drives backdrop-vs-tiles: when
    /// empty, the renderer draws `backdrop`; otherwise the layout tree.
    pub center_is_empty: bool,
    /// The resolved backdrop scene, present only when `center_is_empty`
    /// and the surface declares a `backdrop` view. A normal
    /// `StudioSceneModel` the generic scene renderer draws — the
    /// background is content, never a renderer enum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backdrop: Option<StudioSceneModel>,
    pub tile_count: usize,
    #[serde(default)]
    pub docks: StudioDockPlaneVm,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StudioDockPlaneVm {
    pub top: Option<StudioDockTileVm>,
    pub bottom: Option<StudioDockTileVm>,
    pub left: Option<StudioDockTileVm>,
    pub right: Option<StudioDockTileVm>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioDockTileVm {
    pub edge: StudioDockEdge,
    pub title: String,
    pub size: u16,
    /// The bound view (every slot is a view instance; input is no longer a
    /// dock-content variant).
    pub view: StudioViewVm,
    /// Present when this instance declares an `input` block: the prompt
    /// renderers draw (target strip, buffer, cursor, completion). Any
    /// widget may carry a prompt — input is an orthogonal capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<StudioInputVm>,
}

/// The projection of a view instance's active input buffer. Shape
/// preserved from the deleted Input dock variant; re-sourced from the
/// instance's transient buffer rather than a dock content variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioInputVm {
    pub text: String,
    pub cursor: usize,
    /// Target strip: `target_label` if authored, else derived from the
    /// bound submit target.
    pub route_label: String,
    pub placeholder: String,
    pub hint: String,
    pub submit_enabled: bool,
    /// Completion suggestions from the input's `completion` source.
    #[serde(default)]
    pub completion: Vec<String>,
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
        /// Present when the tile's view declares an `input` block.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<StudioInputVm>,
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
    /// The generic content widget surface: every bound view renders
    /// through rows (typed widget variants arrive with the render pass).
    Rows {
        title: String,
        columns: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provenance: Option<String>,
        #[serde(default)]
        affordance_hints: Vec<String>,
        rows: Vec<StudioRowVm>,
    },
    Timeline {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provenance: Option<String>,
        #[serde(default)]
        affordance_hints: Vec<String>,
        entries: Vec<StudioTimelineEntryVm>,
    },
    Map {
        scene: StudioSceneModel,
    },
    Atlas {
        scene: StudioSceneModel,
    },
    Placeholder {
        title: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StudioTimelineEntryVm {
    Block {
        text: String,
        tone: StudioTone,
    },
    Line {
        primary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<String>,
        tone: StudioTone,
    },
    Pair {
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<String>,
        tone: StudioTone,
        pending: bool,
    },
    Separator {
        label: String,
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
            border: core.style.border.name().to_string(),
            top_bar: top_bar_vm(core),
            status_bar: status_bar_vm(session, chrome, workspace, core, &version),
        },
        metrics: presentation_metrics_vm(core, workspace),
        frame: StudioFrameVm {
            corners: StudioFrameCornersVm {
                visible: true,
                tone: StudioTone::Accent,
            },
        },
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
            .filter(|(index, workspace)| {
                *index == core.active_workspace || !workspace.center_is_empty()
            })
            .map(|(index, workspace)| StudioWorkspaceTabVm {
                number: index + 1,
                active: index == core.active_workspace,
                tile_count: if index == core.active_workspace {
                    core.workspace.tile_ids().len()
                } else {
                    workspace.tile_ids().len()
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
    let total = core.workspace.tile_ids().len();
    let master = core.workspace.tiling.master.count.min(total);
    let slave = total.saturating_sub(master);
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
        .dimension
        .as_ref()
        .map(|dimension| dimension.schedules.total)
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
        key_hint: "ctrl+k open · alt+t/b bars · ctrl+←/→ tab · ctrl+↑/↓ move".to_string(),
    }
}

fn workspace_vm(core: &StudioCore) -> StudioWorkspaceVm {
    let center_is_empty = core.workspace.center_is_empty();
    StudioWorkspaceVm {
        root: core
            .workspace
            .layout()
            .map(|layout| layout_node_vm(&layout, core)),
        focused_tile: tile_id_text(core.workspace.focused_tile),
        center_is_empty,
        // The backdrop scene resolves only on an empty center. The
        // surface's `backdrop` ref selects the scene content; v1 ships the
        // client-side shard builder (the renderer stays generic, so a
        // data/service source later is invisible to it).
        backdrop: center_is_empty.then(|| backdrop_scene(core)).flatten(),
        tile_count: core.workspace.tile_ids().len(),
        docks: dock_plane_vm(core),
    }
}

/// Resolve the backdrop scene from the surface's `backdrop` view ref.
/// For v1 the only backdrop content is the client-side shard scene; the
/// ref selects it. Absent `backdrop` → no scene (the background fill
/// stands).
fn backdrop_scene(core: &StudioCore) -> Option<StudioSceneModel> {
    let backdrop_ref = core
        .data
        .session
        .as_ref()
        .and_then(|session| session.effective_surface.as_ref())
        .and_then(|surface| surface.get("backdrop"))
        .and_then(serde_json::Value::as_str)?;
    // The backdrop is content: read the embedded backdrop VIEW's body and
    // build its scene generically. The surface names which view; the view
    // declares the objects. Swap the ref → swap the background, no Rust.
    let binding = core.views.get(backdrop_ref)?;
    if binding.widget != "scene" {
        return None;
    }
    Some(super::scene_model::scene_from_body(
        &binding.body,
        core.generation,
    ))
}

fn dock_plane_vm(core: &StudioCore) -> StudioDockPlaneVm {
    StudioDockPlaneVm {
        top: dock_tile_vm(core, StudioDockEdge::Top, core.ui.docks.top.as_ref()),
        bottom: dock_tile_vm(core, StudioDockEdge::Bottom, core.ui.docks.bottom.as_ref()),
        left: dock_tile_vm(core, StudioDockEdge::Left, core.ui.docks.left.as_ref()),
        right: dock_tile_vm(core, StudioDockEdge::Right, core.ui.docks.right.as_ref()),
    }
}

fn dock_tile_vm(
    core: &StudioCore,
    edge: StudioDockEdge,
    state: Option<&StudioDockSlotState>,
) -> Option<StudioDockTileVm> {
    let state = state?;
    if !state.visible {
        return None;
    }
    let StudioDockContent::View { view_ref } = &state.content;
    let source_key = super::model::dock_source_key(edge);
    Some(StudioDockTileVm {
        edge,
        title: view_ref.rsplit('/').next().unwrap_or(view_ref).to_string(),
        size: state.size,
        view: bound_view_vm_keyed(core, &source_key, None, view_ref),
        input: instance_input_vm(core, &source_key, view_ref),
    })
}

/// The input prompt VM for a view instance, if its binding declares an
/// `input` block. Re-sources the active transient buffer; layout-neutral.
fn instance_input_vm(
    core: &StudioCore,
    instance_id: &str,
    view_ref: &str,
) -> Option<StudioInputVm> {
    let binding = core.views.get(view_ref)?;
    let input = binding.input.as_ref()?;
    let key = super::model::InputBufferKey::new(instance_id, view_ref, input.id.clone());
    Some(input_vm(core, &key, view_ref, input))
}

/// Render a content-bound view: binding + source response -> widget VM.
/// Pure projection; unknown widgets and missing data degrade honestly.
fn bound_view_vm(core: &StudioCore, tile_id: TileId, view_ref: &str) -> StudioViewVm {
    bound_view_vm_keyed(
        core,
        &tile_id.0.to_string(),
        selected_cursor(core, tile_id),
        view_ref,
    )
}

fn bound_view_vm_keyed(
    core: &StudioCore,
    source_key: &str,
    cursor: Option<usize>,
    view_ref: &str,
) -> StudioViewVm {
    let Some(binding) = core.views.get(view_ref) else {
        return StudioViewVm::Placeholder {
            title: view_ref.to_string(),
            message: format!("view {view_ref} is not embedded in the effective surface"),
        };
    };
    // A binding that failed to parse/validate shows its reason, not its
    // (absent) content — honest degrade, not a silent "not embedded".
    if let Some(reason) = &binding.degraded {
        return StudioViewVm::Placeholder {
            title: view_ref.to_string(),
            message: reason.clone(),
        };
    }
    let response = core.data.sources.get(source_key);
    let title = view_ref.rsplit('/').next().unwrap_or(view_ref).to_string();
    match (binding.widget.as_str(), response) {
        (_, None) => StudioViewVm::Placeholder {
            title,
            message: format!(
                "loading {} …",
                binding
                    .source
                    .as_ref()
                    .map(|s| s.item_ref.as_str())
                    .unwrap_or("(no source)")
            ),
        },
        ("rows", Some(response)) => {
            // Row activation is explicit: the view names the affordance via
            // `selection.activate` (no implicit "first affordance"). The
            // named affordance must be supplied by the `record` producer
            // (binding-time validation) or row activation is unbound.
            let activate_affordance = binding
                .selection
                .as_ref()
                .map(|selection| selection.activate.clone())
                .filter(|affordance_id| {
                    binding
                        .affordances
                        .iter()
                        .find(|a| a.get("id").and_then(|v| v.as_str()) == Some(affordance_id))
                        .is_some_and(|affordance| {
                            super::content::validate_affordance_placeholders(
                                affordance,
                                super::content::Producer::Selection,
                            )
                            .is_ok()
                        })
                });
            let rows = super::content::project_records(binding, response)
                .into_iter()
                .enumerate()
                .map(|(index, record)| StudioRowVm {
                    id: format!("{view_ref}#{index}"),
                    primary: record.primary,
                    secondary: None,
                    meta: record.meta,
                    kind: None,
                    action: activate_affordance.as_ref().map(|affordance_id| {
                        StudioAction::InvokeAffordance {
                            view_ref: view_ref.to_string(),
                            affordance_id: affordance_id.clone(),
                            record: record.raw.clone(),
                        }
                    }),
                    tone: tone_from_name(record.tone.as_deref()),
                    selected: cursor == Some(index),
                })
                .collect();
            StudioViewVm::Rows {
                title,
                columns: Vec::new(),
                provenance: Some(view_ref.to_string()),
                affordance_hints: affordance_hints(binding),
                rows,
            }
        }
        ("timeline", Some(response)) => StudioViewVm::Timeline {
            title,
            provenance: Some(view_ref.to_string()),
            affordance_hints: affordance_hints(binding),
            entries: timeline_entries(super::content::project_records(binding, response)),
        },
        ("key_value" | "text", Some(response)) => {
            let rows = super::content::project_detail(binding, response)
                .into_iter()
                .map(|(key, value)| StudioRowVm {
                    id: format!("{view_ref}#{key}"),
                    primary: format!("{key}: {value}"),
                    secondary: None,
                    meta: None,
                    kind: None,
                    action: None,
                    tone: StudioTone::Neutral,
                    selected: false,
                })
                .collect();
            StudioViewVm::Rows {
                title,
                columns: Vec::new(),
                provenance: Some(view_ref.to_string()),
                affordance_hints: affordance_hints(binding),
                rows,
            }
        }
        (other, Some(_)) => StudioViewVm::Placeholder {
            title,
            // Degradation: an unknown widget never crashes a renderer.
            message: format!("widget `{other}` is not supported by this renderer"),
        },
    }
}

fn affordance_hints(binding: &super::content::ViewBinding) -> Vec<String> {
    binding
        .affordances
        .iter()
        .filter_map(|affordance| {
            affordance
                .get("label")
                .and_then(serde_json::Value::as_str)
                .or_else(|| affordance.get("id").and_then(serde_json::Value::as_str))
        })
        .map(str::to_string)
        .collect()
}

fn timeline_entries(records: Vec<ProjectedRecord>) -> Vec<StudioTimelineEntryVm> {
    let mut entries = Vec::new();
    let mut pending_flow: Option<(String, StudioTone)> = None;
    let mut pending_pairs = std::collections::BTreeMap::<String, usize>::new();

    for record in records {
        match record.role {
            TimelineRole::Flow => {
                if record.primary.is_empty() {
                    continue;
                }
                let tone = tone_from_name(record.tone.as_deref());
                if let Some((text, existing_tone)) = pending_flow.as_mut() {
                    text.push_str(&record.primary);
                    if *existing_tone == StudioTone::Neutral {
                        *existing_tone = tone;
                    }
                } else {
                    pending_flow = Some((record.primary, tone));
                }
            }
            TimelineRole::Boundary => {
                flush_flow(&mut pending_flow, &mut entries);
                entries.push(StudioTimelineEntryVm::Separator {
                    label: record.primary,
                });
            }
            TimelineRole::PairOpen => {
                flush_flow(&mut pending_flow, &mut entries);
                let key = record.pair_key.unwrap_or_default();
                let index = entries.len();
                entries.push(StudioTimelineEntryVm::Pair {
                    summary: record.primary,
                    meta: record.meta,
                    tone: tone_from_name(record.tone.as_deref()),
                    pending: true,
                });
                pending_pairs.insert(key, index);
            }
            TimelineRole::PairClose => {
                flush_flow(&mut pending_flow, &mut entries);
                let Some(key) = record.pair_key.as_deref() else {
                    entries.push(line_entry(record));
                    continue;
                };
                if let Some(index) = pending_pairs.remove(key) {
                    if let Some(StudioTimelineEntryVm::Pair {
                        meta,
                        tone,
                        pending,
                        ..
                    }) = entries.get_mut(index)
                    {
                        *meta = record.meta;
                        *tone = tone_from_name(record.tone.as_deref());
                        *pending = false;
                    }
                } else {
                    entries.push(line_entry(record));
                }
            }
            TimelineRole::Line => {
                flush_flow(&mut pending_flow, &mut entries);
                entries.push(line_entry(record));
            }
        }
    }
    flush_flow(&mut pending_flow, &mut entries);
    entries
}

fn flush_flow(
    pending_flow: &mut Option<(String, StudioTone)>,
    entries: &mut Vec<StudioTimelineEntryVm>,
) {
    if let Some((text, tone)) = pending_flow.take() {
        entries.push(StudioTimelineEntryVm::Block { text, tone });
    }
}

fn line_entry(record: ProjectedRecord) -> StudioTimelineEntryVm {
    StudioTimelineEntryVm::Line {
        primary: record.primary,
        meta: record.meta,
        tone: tone_from_name(record.tone.as_deref()),
    }
}

fn tone_from_name(name: Option<&str>) -> StudioTone {
    match name {
        Some("accent") => StudioTone::Accent,
        Some("good") => StudioTone::Good,
        Some("warn") => StudioTone::Warn,
        Some("danger") => StudioTone::Danger,
        _ => StudioTone::Neutral,
    }
}

/// Project a view instance's input buffer into the prompt VM. The target
/// strip is `target_label` if authored, else derived from the bound submit
/// target (the seat route for `submit: route`; the affordance's invoke
/// target for `submit: <affordance>`).
fn input_vm(
    core: &StudioCore,
    key: &super::model::InputBufferKey,
    view_ref: &str,
    input: &super::content::InputBlock,
) -> StudioInputVm {
    let buffer = core.ui.input_buffers.get(&key.storage_key());
    let text = buffer.map(|b| b.text.clone()).unwrap_or_default();
    let cursor = buffer.map(|b| b.cursor).unwrap_or(0);

    let route = core.seat.fold().input_route();
    let route_label = input
        .target_label
        .clone()
        .map(|label| format!("→ {label}"))
        .unwrap_or_else(|| derived_target_label(core, view_ref, input, &route));

    // Completion suggestions come from the input's `completion` source.
    let completion = input_completion(core, input, &text);
    let hint = completion
        .first()
        .cloned()
        .unwrap_or_else(|| "Shift+Enter submit · / for commands".to_string());

    let has_route_target =
        input.submits_to_route() && (text.starts_with('/') || route.has_target());
    let has_affordance_target = input.submit_affordance().is_some();
    StudioInputVm {
        cursor,
        route_label,
        placeholder: input
            .placeholder
            .clone()
            .unwrap_or_else(|| "type RyeOS input…".to_string()),
        hint,
        submit_enabled: !text.trim().is_empty() && (has_route_target || has_affordance_target),
        completion,
        text,
    }
}

/// Derive the target strip when the author gives no `target_label`.
fn derived_target_label(
    core: &StudioCore,
    view_ref: &str,
    input: &super::content::InputBlock,
    route: &super::seat::InputRoute,
) -> String {
    if let Some(affordance_id) = input.submit_affordance() {
        // Derive from the bound affordance's invoke target.
        if let Some(target) = core
            .views
            .get(view_ref)
            .and_then(|binding| affordance_invoke_target(binding, affordance_id))
        {
            return format!("→ {target}");
        }
        return format!("→ {affordance_id}");
    }
    // `submit: route` — render the seat route truthfully.
    match (&route.invoke, &route.thread) {
        (None, _) => "no target — surface declares no route".to_string(),
        (Some(_), Some(thread)) => format!("→ chained on {thread}"),
        (Some(InvokeTemplate::Service { item_ref }), None) => format!("→ {item_ref} (new chain)"),
        (Some(InvokeTemplate::Command { tokens }), None) => {
            format!("→ /{} (new chain)", tokens.join(" "))
        }
        (Some(InvokeTemplate::UiFacet { key }), None) => format!("→ {key}"),
    }
}

fn affordance_invoke_target(
    binding: &super::content::ViewBinding,
    affordance_id: &str,
) -> Option<String> {
    let affordance = binding
        .affordances
        .iter()
        .find(|a| a.get("id").and_then(|v| v.as_str()) == Some(affordance_id))?;
    let invoke = affordance.get("invoke")?;
    match invoke.get("plane").and_then(serde_json::Value::as_str)? {
        "rye" => invoke
            .get("ref")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                invoke
                    .get("tokens")
                    .and_then(serde_json::Value::as_array)
                    .map(|tokens| {
                        format!(
                            "/{}",
                            tokens
                                .iter()
                                .filter_map(serde_json::Value::as_str)
                                .collect::<Vec<_>>()
                                .join(" ")
                        )
                    })
            }),
        "ui" => invoke
            .get("facet")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

/// Completion suggestions from the input's `completion` source. For the
/// `service:commands/list` grammar this is slash-mode next-token
/// candidates; pure projection over open JSON.
fn input_completion(
    core: &StudioCore,
    input: &super::content::InputBlock,
    text: &str,
) -> Vec<String> {
    let Some(completion) = input.completion.as_ref() else {
        return Vec::new();
    };
    // The command grammar is fetched into `core.data.commands`.
    if completion.item_ref != "service:commands/list" {
        return Vec::new();
    }
    let Some(records) = core
        .data
        .commands
        .as_ref()
        .and_then(|data| data.get("commands"))
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    super::tokenize::slash_completion_hint(records, text)
        .into_iter()
        .collect()
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
            let input = core
                .workspace
                .tiles
                .get(tile_id)
                .and_then(|tile| match &tile.view {
                    ViewSpec::Bound { view_ref } => {
                        instance_input_vm(core, &tile_id.0.to_string(), view_ref)
                    }
                    _ => None,
                });
            StudioLayoutNodeVm::Tile {
                tile_id: tile_id_text(*tile_id),
                focused: *tile_id == core.workspace.focused_tile,
                title,
                actions: tile_actions(core, *tile_id),
                view,
                input,
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
        ViewSpec::Bound { view_ref } => bound_view_vm(core, tile_id, view_ref),
        ViewSpec::Atlas => StudioViewVm::Atlas {
            scene: build_scene_model(core),
        },
        ViewSpec::Graph { .. } => StudioViewVm::Map {
            scene: build_scene_model(core),
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

/// Launchable views: the surface's embedded content library plus the
/// two engine ambient views (graph topology, atlas). Nothing here names
/// a product concept — labels and hints come from the view items.
pub(crate) fn launcher_items(core: &StudioCore) -> Vec<StudioLauncherItemVm> {
    let mut items: Vec<StudioLauncherItemVm> = [
        (
            "Graph",
            "RyeOS topology",
            ViewSpec::Graph { graph_id: None },
        ),
        ("Atlas", "2D namespace map", ViewSpec::Atlas),
    ]
    .into_iter()
    .map(|(label, hint, view)| StudioLauncherItemVm {
        label: label.to_string(),
        hint: hint.to_string(),
        action: StudioAction::OpenView { view: view.clone() },
        secondary_action: Some(StudioAction::OpenNewView { view }),
        enabled: true,
    })
    .collect();
    for (view_ref, binding) in &core.views {
        let view = ViewSpec::Bound {
            view_ref: view_ref.clone(),
        };
        items.push(StudioLauncherItemVm {
            label: view_ref
                .strip_prefix("view:")
                .unwrap_or(view_ref)
                .to_string(),
            hint: binding
                .description
                .clone()
                .unwrap_or_else(|| binding.widget.clone()),
            action: StudioAction::OpenView { view: view.clone() },
            secondary_action: Some(StudioAction::OpenNewView { view }),
            enabled: true,
        });
    }
    items
}

pub(crate) fn launcher_items_for(core: &StudioCore) -> Vec<StudioLauncherItemVm> {
    let mut items = context_launcher_items(core);
    items.extend(dock_launcher_items(core));
    items.extend(launcher_items(core));
    items
}

fn dock_launcher_items(core: &StudioCore) -> Vec<StudioLauncherItemVm> {
    // Only surface-declared slots are toggleable; absent edges have no
    // slot and offer nothing. Labels stay mechanism words (edge names).
    [
        (
            StudioDockEdge::Bottom,
            "bottom",
            core.ui.docks.bottom.as_ref(),
        ),
        (StudioDockEdge::Left, "left", core.ui.docks.left.as_ref()),
        (StudioDockEdge::Right, "right", core.ui.docks.right.as_ref()),
        (StudioDockEdge::Top, "top", core.ui.docks.top.as_ref()),
    ]
    .into_iter()
    .filter_map(|(edge, name, slot)| slot.map(|slot| (edge, name, slot.visible)))
    .map(|(edge, name, visible)| StudioLauncherItemVm {
        label: format!("{} {name} slot", if visible { "Hide" } else { "Show" }),
        hint: "toggle edge slot".to_string(),
        action: StudioAction::ToggleDock { edge },
        secondary_action: None,
        enabled: true,
    })
    .collect()
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

fn focused_selection_hint(core: &StudioCore) -> Option<String> {
    let tile_id = core.workspace.focused_tile;
    let view = core.workspace.focused_view()?;
    let cursor = selected_cursor(core, tile_id).unwrap_or(0);
    let row = focused_rows(core, view, tile_id).get(cursor)?.clone();
    Some(row.secondary.or(row.meta).unwrap_or(row.primary))
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
    // Dynamic tiling: the algorithm owns the tree; tiles offer no
    // manual splits. Closing the last tile returns home.
    let _ = core;
    let tile_id = tile_id_text(tile_id);
    vec![StudioTileActionVm {
        label: "×".to_string(),
        title: "Close tile".to_string(),
        action: StudioAction::CloseTile { tile_id },
    }]
}

pub(crate) fn action_for_focused_row(core: &StudioCore) -> Option<StudioAction> {
    let tile_id = core.workspace.focused_tile;
    let view = core.workspace.focused_view()?;
    let cursor = selected_cursor(core, tile_id).unwrap_or(0);
    let rows = focused_rows(core, view, tile_id);
    rows.get(cursor).and_then(|row| row.action.clone())
}

/// Rows of the focused tile, regardless of how it is bound. Bound views
/// project their fetched source; the thread view projects the threads
/// DTO; ambient views have no rows.
fn focused_rows(core: &StudioCore, view: &ViewSpec, tile_id: TileId) -> Vec<StudioRowVm> {
    match view {
        ViewSpec::Bound { view_ref } => match bound_view_vm(core, tile_id, view_ref) {
            StudioViewVm::Rows { rows, .. } => rows,
            _ => Vec::new(),
        },
        ViewSpec::Atlas | ViewSpec::Graph { .. } => Vec::new(),
    }
}

fn selected_cursor(core: &StudioCore, tile_id: TileId) -> Option<usize> {
    let tile = core.workspace.tiles.get(&tile_id)?;
    match &tile.local {
        ViewLocalState::GenericList { cursor, .. } => Some(*cursor),
        ViewLocalState::None => None,
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

fn tile_id_text(id: TileId) -> String {
    id.0.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(
        primary: &str,
        meta: Option<&str>,
        tone: Option<&str>,
        role: TimelineRole,
        pair_key: Option<&str>,
    ) -> ProjectedRecord {
        ProjectedRecord {
            primary: primary.to_string(),
            meta: meta.map(str::to_string),
            tone: tone.map(str::to_string),
            role,
            pair_key: pair_key.map(str::to_string),
            raw: json!({ "primary": primary }),
        }
    }

    #[test]
    fn timeline_entries_merge_consecutive_flow_records() {
        let entries = timeline_entries(vec![
            record("hello ", None, None, TimelineRole::Flow, None),
            record("world", None, Some("accent"), TimelineRole::Flow, None),
        ]);

        assert_eq!(
            entries,
            vec![StudioTimelineEntryVm::Block {
                text: "hello world".to_string(),
                tone: StudioTone::Accent,
            }]
        );
    }

    #[test]
    fn empty_center_resolves_backdrop_scene_from_surface_ref() {
        let session = crate::studio::model::BrowserSession {
            session_id: "S-backdrop".to_string(),
            surface_ref: "surface:ryeos/studio/base".to_string(),
            effective_surface: Some(json!({
                "name": "studio-base",
                "version": "1.0.0",
                "backdrop": "view:ryeos/backdrop/shard",
                // The backdrop is content: its scene objects live in the
                // embedded view's body, not in Rust.
                "views": {
                    "view:ryeos/backdrop/shard": {
                        "widget": "scene",
                        "body": { "objects": [
                            { "kind": "particle", "position": [1.0, 2.0], "scale": 0.9, "color": "#d65d0e", "tone": "accent" },
                            { "kind": "text", "position": [0.0, -8.0], "label": "RYE OS", "color": "#d65d0e", "tone": "accent" }
                        ] }
                    }
                }
            })),
            ..Default::default()
        };
        let core = StudioCore::new(session, crate::studio::model::BrowserViewport::default(), 0);
        let vm = build_view_model(&core);

        assert!(vm.workspace.center_is_empty);
        let backdrop = vm
            .workspace
            .backdrop
            .expect("backdrop scene on empty center");
        // The scene resolves from the view body — objects incl. text labels.
        assert!(!backdrop.objects.is_empty());
        assert!(backdrop
            .objects
            .iter()
            .any(|o| o.label.as_deref() == Some("RYE OS")));
    }

    #[test]
    fn no_backdrop_ref_means_no_backdrop_scene() {
        let session = crate::studio::model::BrowserSession {
            session_id: "S-nobackdrop".to_string(),
            surface_ref: "surface:ryeos/studio/base".to_string(),
            effective_surface: Some(json!({ "name": "studio-base", "views": {} })),
            ..Default::default()
        };
        let core = StudioCore::new(session, crate::studio::model::BrowserViewport::default(), 0);
        let vm = build_view_model(&core);
        assert!(vm.workspace.center_is_empty);
        assert!(vm.workspace.backdrop.is_none());
    }

    #[test]
    fn surface_style_border_flows_into_presentation_chrome() {
        let session = crate::studio::model::BrowserSession {
            session_id: "S-border".to_string(),
            surface_ref: "surface:ryeos/studio/base".to_string(),
            effective_surface: Some(json!({
                "name": "studio-base",
                "style": { "border": "thick" }
            })),
            ..Default::default()
        };
        let core = StudioCore::new(session, crate::studio::model::BrowserViewport::default(), 0);
        let vm = build_view_model(&core);
        assert_eq!(vm.presentation.chrome.border, "thick");

        // Absent style defaults to thin.
        let default_vm = build_view_model(&StudioCore::default());
        assert_eq!(default_vm.presentation.chrome.border, "thin");
    }

    #[test]
    fn timeline_entries_collapse_matching_pairs_in_open_position() {
        let entries = timeline_entries(vec![
            record(
                "run tool",
                None,
                Some("accent"),
                TimelineRole::PairOpen,
                Some("call-1"),
            ),
            record("thinking", None, None, TimelineRole::Line, None),
            record(
                "ok",
                Some("42ms"),
                Some("good"),
                TimelineRole::PairClose,
                Some("call-1"),
            ),
        ]);

        assert_eq!(
            entries,
            vec![
                StudioTimelineEntryVm::Pair {
                    summary: "run tool".to_string(),
                    meta: Some("42ms".to_string()),
                    tone: StudioTone::Good,
                    pending: false,
                },
                StudioTimelineEntryVm::Line {
                    primary: "thinking".to_string(),
                    meta: None,
                    tone: StudioTone::Neutral,
                },
            ]
        );
    }

    #[test]
    fn timeline_entries_close_without_open_degrades_to_line() {
        let entries = timeline_entries(vec![record(
            "orphan close",
            Some("done"),
            Some("good"),
            TimelineRole::PairClose,
            Some("call-1"),
        )]);

        assert_eq!(
            entries,
            vec![StudioTimelineEntryVm::Line {
                primary: "orphan close".to_string(),
                meta: Some("done".to_string()),
                tone: StudioTone::Good,
            }]
        );
    }

    #[test]
    fn timeline_entries_open_without_close_stays_pending() {
        let entries = timeline_entries(vec![record(
            "run tool",
            None,
            Some("accent"),
            TimelineRole::PairOpen,
            Some("call-1"),
        )]);

        assert_eq!(
            entries,
            vec![StudioTimelineEntryVm::Pair {
                summary: "run tool".to_string(),
                meta: None,
                tone: StudioTone::Accent,
                pending: true,
            }]
        );
    }

    #[test]
    fn timeline_entries_boundary_becomes_separator() {
        let entries = timeline_entries(vec![record(
            "turn 2",
            None,
            None,
            TimelineRole::Boundary,
            None,
        )]);

        assert_eq!(
            entries,
            vec![StudioTimelineEntryVm::Separator {
                label: "turn 2".to_string(),
            }]
        );
    }

    #[test]
    fn timeline_entries_records_with_no_role_map_are_lines() {
        let binding: super::super::content::ViewBinding = serde_json::from_value(json!({
            "widget": "timeline",
            "source": { "ref": "service:events/chain_replay", "collection": "events" },
            "projections": { "default": { "primary": "event_type" } }
        }))
        .unwrap();
        let records = super::super::content::project_records(
            &binding,
            &json!({ "events": [ { "event_type": "message" } ] }),
        );

        assert_eq!(
            timeline_entries(records),
            vec![StudioTimelineEntryVm::Line {
                primary: "message".to_string(),
                meta: None,
                tone: StudioTone::Neutral,
            }]
        );
    }

    #[test]
    fn timeline_entries_skip_flow_records_without_primary() {
        let entries = timeline_entries(vec![
            record("", None, None, TimelineRole::Flow, None),
            record("durable", None, None, TimelineRole::Flow, None),
        ]);

        assert_eq!(
            entries.as_slice(),
            &[StudioTimelineEntryVm::Block {
                text: "durable".into(),
                tone: StudioTone::Neutral,
            }]
        );
    }

    #[test]
    fn actual_chain_timeline_binding_projects_replay_shapes() {
        let binding: super::super::content::ViewBinding = serde_yaml::from_str(include_str!(
            "../../../../../bundles/studio/.ai/views/ryeos/chain/timeline.yaml"
        ))
        .unwrap();
        let records = super::super::content::project_records(
            &binding,
            &json!({ "events": [
                { "event_type": "cognition_in", "payload": { "turn": 1 } },
                { "event_type": "cognition_out", "payload": { "content": "answer", "turn": 1 } },
                { "event_type": "cognition_out", "payload": { "delta": "live-only" } },
                { "event_type": "tool_call_start", "payload": { "tool": "tool:demo", "call_id": "call-1" } },
                { "event_type": "tool_call_result", "payload": { "tool": "tool:demo", "call_id": "call-1", "result_size_bytes": 42 } }
            ] }),
        );

        let entries = timeline_entries(records);
        assert!(entries.iter().any(
            |entry| matches!(entry, StudioTimelineEntryVm::Block { text, .. } if text == "answer")
        ));
        assert!(entries.iter().any(|entry| matches!(
            entry,
            StudioTimelineEntryVm::Separator { label } if label == "1"
        )));
        assert!(entries.iter().any(|entry| matches!(
            entry,
            StudioTimelineEntryVm::Pair { summary, tone: StudioTone::Good, pending: false, .. } if summary == "tool:demo"
        )));
        assert!(!entries.iter().any(|entry| matches!(
            entry,
            StudioTimelineEntryVm::Line { primary, .. } if primary.contains("live-only")
        )));
    }

    #[test]
    fn timeline_entries_unknown_role_degrades_to_line() {
        let binding: super::super::content::ViewBinding = serde_json::from_value(json!({
            "widget": "timeline",
            "source": { "ref": "service:events/chain_replay", "collection": "events" },
            "projections": {
                "event_kinds": { "message": { "primary": "event_type", "role": "unknown" } },
                "default": { "primary": "event_type" }
            }
        }))
        .unwrap();
        let records = super::super::content::project_records(
            &binding,
            &json!({ "events": [ { "event_type": "message" } ] }),
        );

        assert_eq!(records[0].role, TimelineRole::Line);
        assert!(matches!(
            timeline_entries(records).as_slice(),
            [StudioTimelineEntryVm::Line { primary, .. }] if primary == "message"
        ));
    }

    fn input_session(view: serde_json::Value) -> crate::studio::model::StudioCore {
        let session = crate::studio::model::BrowserSession {
            effective_surface: Some(json!({
                "name": "t",
                "slots": { "bottom": { "content": "view:ryeos/input", "open": true, "size": 7 } },
                "views": { "view:ryeos/input": view }
            })),
            ..Default::default()
        };
        StudioCore::new(session, crate::studio::model::BrowserViewport::default(), 0)
    }

    #[test]
    fn input_view_renders_prompt_in_bottom_slot() {
        let core = input_session(json!({
            "widget": "text",
            "input": { "id": "line", "placeholder": "Ask or run a command", "submit": "route" }
        }));
        let vm = build_view_model(&core);
        let bottom = vm.workspace.docks.bottom.expect("bottom slot");
        let input = bottom.input.expect("bottom instance declares input");
        assert_eq!(input.placeholder, "Ask or run a command");
    }

    #[test]
    fn target_label_override_wins_over_derived_strip() {
        let core = input_session(json!({
            "widget": "text",
            "input": { "id": "line", "target_label": "thread input", "submit": "route" }
        }));
        let vm = build_view_model(&core);
        let input = vm.workspace.docks.bottom.unwrap().input.unwrap();
        assert_eq!(input.route_label, "→ thread input");
    }

    #[test]
    fn completion_uses_the_inputs_completion_source() {
        let mut core = input_session(json!({
            "widget": "text",
            "input": {
                "id": "line",
                "submit": "route",
                "completion": { "ref": "service:commands/list", "collection": "commands" }
            }
        }));
        core.data.commands = Some(json!({
            "commands": [
                { "invocable": true, "tokens": ["thread", "list"], "description": "List threads" },
                { "invocable": true, "tokens": ["thread", "get"], "description": "Get thread" }
            ]
        }));
        core.ui.input_buffers.insert(
            crate::studio::model::InputBufferKey::new("dock:bottom", "view:ryeos/input", "line")
                .storage_key(),
            crate::studio::model::StudioInputState {
                text: "/thread ".to_string(),
                cursor: "/thread ".len(),
            },
        );
        let vm = build_view_model(&core);
        let input = vm.workspace.docks.bottom.unwrap().input.unwrap();
        assert!(
            input
                .completion
                .iter()
                .any(|s| s.contains("get") && s.contains("list")),
            "completion lists next slash tokens: {:?}",
            input.completion
        );
    }

    #[test]
    fn input_without_completion_source_has_no_suggestions() {
        let mut core = input_session(json!({
            "widget": "text",
            "input": { "id": "line", "submit": "route" }
        }));
        core.data.commands = Some(json!({ "commands": [
            { "invocable": true, "tokens": ["thread", "list"] }
        ] }));
        core.ui.input_buffers.insert(
            crate::studio::model::InputBufferKey::new("dock:bottom", "view:ryeos/input", "line")
                .storage_key(),
            crate::studio::model::StudioInputState {
                text: "/thr".to_string(),
                cursor: 4,
            },
        );
        let vm = build_view_model(&core);
        let input = vm.workspace.docks.bottom.unwrap().input.unwrap();
        assert!(
            input.completion.is_empty(),
            "no completion source -> no suggestions"
        );
    }
}
