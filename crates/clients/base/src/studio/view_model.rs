use serde::{Deserialize, Serialize};

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
    #[serde(default)]
    pub help: StudioHelpVm,
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
    /// This input is a live filter (feeds a source param, no submit): the
    /// renderer composes it as a filter line above its widget rather than
    /// replacing the widget, and Enter activates the focused row.
    #[serde(default)]
    pub live_filter: bool,
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
        /// The entry under the point (absolute index into `entries`), for
        /// highlight + scroll. Derived from the tile cursor, which the feed
        /// reads as distance-from-bottom (0 = newest), so the point sits at
        /// the tail by default and arrow-up walks back into history.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selected: Option<usize>,
        /// The foldable turn-section the point sits in, if any — what a fold
        /// key toggles. `None` when the point is in unfoldable content.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fold_section: Option<usize>,
    },
    Map {
        scene: StudioSceneModel,
    },
    Atlas {
        scene: StudioSceneModel,
    },
    /// A foldable multi-section list — the magit-style status surface: one
    /// widget over many datasets, each section a titled, collapsible group of
    /// rows. The engine knows the `sections` widget vocabulary; the specific
    /// sections (threads/bundles/node/…) are declared by the bound view, never
    /// named here (no fire-sword). Rows reuse `StudioRowVm`, so per-row actions
    /// come for free.
    Sections {
        title: String,
        sections: Vec<StudioSectionVm>,
        /// The section the point sits in — what a fold key toggles. `None`
        /// when there is no point (empty view).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fold_section: Option<usize>,
    },
    /// A real column table — the typed list surface for the non-chat lenses
    /// (threads/bundles/schedules/…): aligned cells under headers rather than
    /// the rows widget's primary/secondary/meta. The engine knows the `table`
    /// widget vocabulary; the columns + their field projections are declared by
    /// the bound view (no fire-sword). Rows carry per-row actions like the rows
    /// widget.
    Table {
        title: String,
        columns: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provenance: Option<String>,
        #[serde(default)]
        affordance_hints: Vec<String>,
        rows: Vec<StudioTableRowVm>,
    },
    Placeholder {
        title: String,
        message: String,
    },
}

/// One row of a `Table` view: a cell per declared column, plus the per-row
/// tone/action/selection the rows widget also carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioTableRowVm {
    pub id: String,
    pub cells: Vec<String>,
    pub tone: StudioTone,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<StudioAction>,
    #[serde(default)]
    pub selected: bool,
    /// The row's raw record — base-only (not serialized to clients). The launcher
    /// rebuilds the row's non-activate affordances (e.g. Cancel) from it, so row
    /// management is reachable. Skipped to avoid duplicating every record into
    /// the per-row wire payload.
    #[serde(skip)]
    pub raw: serde_json::Value,
}

/// One titled, collapsible group within a `Sections` view. `count` is the
/// section's total even when `collapsed` hides the rows, so the header can
/// report it without the rows being present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioSectionVm {
    pub title: String,
    pub count: usize,
    #[serde(default)]
    pub collapsed: bool,
    /// The point is on this section's header. Only a collapsed section's
    /// header is a point (it has no visible rows to land on); the highlight
    /// lets the operator see what a fold key would re-expand.
    #[serde(default)]
    pub header_selected: bool,
    pub rows: Vec<StudioRowVm>,
}

// The timeline entry shapes live in `super::timeline`; re-exported here so
// the established `studio::view_model::StudioTimelineEntryVm` path is stable.
pub use super::timeline::StudioTimelineEntryVm;

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

/// The keys/help overlay: a static catalogue of the global key bindings,
/// grouped by category. A meta-overlay (discoverability), not braid content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StudioHelpVm {
    pub open: bool,
    pub entries: Vec<StudioHelpEntryVm>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioHelpEntryVm {
    pub category: String,
    pub keys: String,
    pub description: String,
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
        help: help(core),
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
        .map(|tile| tile_title(core, &tile.view))
        .unwrap_or_else(|| "home".to_string())
}

/// Tile/launcher title for a bound view: prefer the view item's authored
/// `name:` (content), fall back to the ref tail when none is declared. The
/// authored label is content too — "views are content" extends to the title.
fn tile_title(core: &StudioCore, view: &crate::workspace::ViewSpec) -> String {
    core.views
        .get(&view.view_ref)
        .and_then(|binding| binding.name.as_deref())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| view.title())
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
    let scene_object_count = build_scene_model(core, &core.ui.atlas, None, None)
        .objects
        .len();
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

/// Cumulative token usage for the loaded conversation: the sum of
/// `thread_usage` input/output tokens across the braid events in any fetched
/// source. Only the chain-replay (timeline) source carries `thread_usage`, so
/// summing across sources yields the conversation total; `(0, 0)` when none.
fn conversation_usage(core: &StudioCore) -> (u64, u64) {
    let mut input = 0u64;
    let mut output = 0u64;
    for source in core.data.sources.values() {
        let Some(events) = source.get("events").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for event in events {
            if event.get("event_type").and_then(serde_json::Value::as_str) != Some("thread_usage") {
                continue;
            }
            if let Some(payload) = event.get("payload") {
                input += payload
                    .get("input_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                output += payload
                    .get("output_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
            }
        }
    }
    (input, output)
}

/// Compact a token count for the status strip (`1234` → `1.2k`).
fn compact_count(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
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
    let (usage_in, usage_out) = conversation_usage(core);
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
            // Always-visible budget: the conversation's cumulative tokens
            // (input/output) summed from the braid's thread_usage events.
            StudioStatusSegmentVm {
                id: "usage".to_string(),
                label: Some("tokens".to_string()),
                value: format!("{}/{}", compact_count(usage_in), compact_count(usage_out)),
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
        view: bound_view_vm_keyed(core, &source_key, None, None, view_ref, &core.ui.atlas),
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
        selected_collapsed(core, tile_id),
        view_ref,
        core.tile_atlas_state(tile_id),
    )
}

fn bound_view_vm_keyed(
    core: &StudioCore,
    source_key: &str,
    cursor: Option<usize>,
    collapsed: Option<&std::collections::BTreeSet<usize>>,
    view_ref: &str,
    atlas: &crate::atlas::AtlasUiStateVm,
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
    // Engine scene widgets render from shared topology/item data + this
    // tile's atlas arrangement, not from a per-tile source response — so
    // they dispatch before the source-required arms below.
    match binding.widget.as_str() {
        "atlas" => {
            // This tile's own scoped dataset when it has one (keyed by tile
            // id == source_key); otherwise None falls back to the shared data.
            return StudioViewVm::Atlas {
                scene: build_scene_model(
                    core,
                    atlas,
                    core.data.tile_items.get(source_key),
                    core.data.tile_file_space.get(source_key),
                ),
            };
        }
        "graph" => {
            // Graph renders shared topology; no per-tile content scope yet.
            return StudioViewVm::Map {
                scene: build_scene_model(core, atlas, None, None),
            };
        }
        "sections" => {
            // A sections view reads one source per section (keyed by
            // section_source_key), not the single per-tile response below — so
            // it assembles here, ahead of the source-required arms.
            let title = view_ref.rsplit('/').next().unwrap_or(view_ref).to_string();
            if binding.sections.is_empty() {
                return StudioViewVm::Placeholder {
                    title,
                    message: "sections view declares no sections".to_string(),
                };
            }
            // One flat point list walks top-down across sections: an expanded
            // section contributes its rows, a collapsed section contributes a
            // single header point (so it stays addressable to re-expand). The
            // tile cursor addresses that flat index; `fold_section` is the
            // section the point lands in — what a fold key toggles.
            let folds = collapsed;
            let mut flat = 0usize;
            let mut fold_section = None;
            let mut sections = Vec::with_capacity(binding.sections.len());
            for (index, section) in binding.sections.iter().enumerate() {
                let is_collapsed = folds.is_some_and(|set| set.contains(&index));
                // Row activation per section: the section names an affordance
                // (in the host binding's `affordances`) the same way the rows
                // widget's `selection.activate` does — validated identically.
                let activate = section.activate.as_ref().filter(|affordance_id| {
                    binding
                        .affordances
                        .iter()
                        .find(|a| a.get("id").and_then(|v| v.as_str()) == Some(affordance_id.as_str()))
                        .is_some_and(|affordance| {
                            super::content::validate_affordance_placeholders(
                                affordance,
                                super::content::Producer::Selection,
                            )
                            .is_ok()
                        })
                });
                let key = super::content::section_source_key(source_key, index);
                let records = core
                    .data
                    .sources
                    .get(&key)
                    .map(|response| super::content::project_section(section, response))
                    .unwrap_or_default();
                let count = records.len();
                let mut header_selected = false;
                let mut rows = Vec::new();
                if is_collapsed {
                    // The collapsed section is one point: its header.
                    if cursor == Some(flat) {
                        header_selected = true;
                        fold_section = Some(index);
                    }
                    flat += 1;
                } else {
                    rows.reserve(count);
                    for record in records {
                        let selected = cursor == Some(flat);
                        if selected {
                            fold_section = Some(index);
                        }
                        flat += 1;
                        rows.push(StudioRowVm {
                            id: format!("{view_ref}#{index}#{}", rows.len()),
                            primary: record.primary,
                            secondary: None,
                            meta: record.meta,
                            kind: None,
                            action: activate.map(|affordance_id| StudioAction::InvokeAffordance {
                                view_ref: view_ref.to_string(),
                                affordance_id: affordance_id.clone(),
                                record: record.raw.clone(),
                            }),
                            tone: tone_from_name(record.tone.as_deref()),
                            selected,
                        });
                    }
                }
                sections.push(StudioSectionVm {
                    title: section.title.clone(),
                    count,
                    collapsed: is_collapsed,
                    header_selected,
                    rows,
                });
            }
            return StudioViewVm::Sections {
                title,
                sections,
                fold_section,
            };
        }
        _ => {}
    }
    // A view may render a seat facet directly (no service fetch) — e.g. the
    // inspector showing `selection.summary`, an inline event detail written by
    // an inspect action. The facet wins when it resolves; otherwise the view
    // falls back to its fetched `source` response.
    let facet_response = binding
        .facet
        .as_deref()
        .and_then(|facet| facet_backed_response(core, facet));
    let response = facet_response
        .as_ref()
        .or_else(|| core.data.sources.get(source_key));
    let title = view_ref.rsplit('/').next().unwrap_or(view_ref).to_string();
    match (binding.widget.as_str(), response) {
        // A feed with no chain root is empty, not loading — it would spin
        // forever on a fetch that never resolves (no chain root to replay).
        // The feed follows `chain_root` (the whole braid), so key the empty
        // state off that, not the moving head. Show an honest
        // start-a-conversation state instead.
        ("timeline", None) if core.seat.fold().input_route().chain_root.is_none() => {
            StudioViewVm::Placeholder {
                title,
                message: "No conversation yet — type below to start one.".to_string(),
            }
        }
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
            let activate_affordance = activate_affordance(binding);
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
        ("timeline", Some(response)) => {
            let mut full = timeline_entries(super::content::project_records(binding, response));
            // Deep-watch header: a chain execution summary line at the top of the
            // braid, from the source's `summary` (chain_replay). Absent for any
            // timeline whose source carries no `summary`, so it never intrudes.
            if let Some(summary) = timeline_summary_entry(response) {
                full.insert(0, summary);
            }
            append_live_delta(core, &mut full);
            // Apply the operator's folds, then project over the VISIBLE list so
            // the cursor, scroll, and point all address what's actually shown.
            let empty = std::collections::BTreeSet::new();
            let folded = super::timeline::fold_timeline(full, collapsed.unwrap_or(&empty));
            // The feed reads the tile cursor as distance-from-bottom: 0 keeps
            // the point on the newest entry (so it follows the live tail),
            // larger values walk back into history. Empty feed → no point.
            let selected = folded.entries.len().checked_sub(1).map(|last| {
                let distance = cursor.unwrap_or(0).min(last);
                last - distance
            });
            // The foldable section under the point — what a fold key toggles.
            let fold_section = selected.and_then(|i| {
                let section = folded.sections.get(i).copied()?;
                folded.collapsible.contains(&section).then_some(section)
            });
            StudioViewVm::Timeline {
                title,
                provenance: Some(view_ref.to_string()),
                affordance_hints: affordance_hints(binding),
                entries: folded.entries,
                selected,
                fold_section,
            }
        }
        ("table", Some(response)) => {
            // The typed list surface: columns + per-column field projections,
            // declared by the binding (the engine knows only the `table`
            // widget). Row activation is the same explicit `selection.activate`
            // affordance the rows widget uses.
            let activate_affordance = activate_affordance(binding);
            let columns = super::content::table_columns(binding);
            let column_labels = columns.iter().map(|col| col.label.clone()).collect();
            let rows = super::content::project_table(binding, response, &columns)
                .into_iter()
                .enumerate()
                .map(|(index, record)| StudioTableRowVm {
                    id: format!("{view_ref}#{index}"),
                    cells: record.cells,
                    tone: tone_from_name(record.tone.as_deref()),
                    action: activate_affordance.as_ref().map(|affordance_id| {
                        StudioAction::InvokeAffordance {
                            view_ref: view_ref.to_string(),
                            affordance_id: affordance_id.clone(),
                            record: record.raw.clone(),
                        }
                    }),
                    selected: cursor == Some(index),
                    raw: record.raw,
                })
                .collect();
            StudioViewVm::Table {
                title,
                columns: column_labels,
                provenance: Some(view_ref.to_string()),
                affordance_hints: affordance_hints(binding),
                rows,
            }
        }
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

/// The affordance a row's activation invokes, shared by the rows and table
/// widgets. Activation is explicit — the view names it via `selection.activate`
/// (no implicit "first affordance") — and the named affordance must be
/// supplied by the `record` producer (binding-time validation) or activation is
/// unbound (`None`).
fn activate_affordance(binding: &super::content::ViewBinding) -> Option<String> {
    binding
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
        })
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

// Timeline entry building + the live cognition buffer render live in
// `super::timeline`; re-exported crate-wide so the timeline arm above and the
// tests below call them unqualified (via `use super::*`).
pub(crate) use super::timeline::{append_live_delta, timeline_entries};

pub(crate) fn tone_from_name(name: Option<&str>) -> StudioTone {
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

    // Completion suggestions: an inline @-mention hint when the cursor is in
    // one, else the input's `/` command grammar.
    let completion = input_completion(core, view_ref, input, &text, cursor);
    let live_filter = input.is_live_filter();
    // Cyclable live filters name their active field in the prompt and hint.
    let feeds = input.feeds.as_ref();
    let filter_field = buffer.map(|b| b.filter_field).unwrap_or(0);
    let cyclable = feeds.is_some_and(|f| f.field_count() > 1);
    let hint = completion.first().cloned().unwrap_or_else(|| {
        if live_filter && cyclable {
            "Tab: field · type to filter · Enter opens".to_string()
        } else if live_filter {
            "type to filter · ↑↓ select · Enter opens".to_string()
        } else {
            "Shift+Enter submit · / for commands · @ for refs".to_string()
        }
    });
    // A cyclable filter's prompt names the active field ("filter by source…");
    // otherwise the authored placeholder, else a generic prompt.
    let placeholder = feeds
        .and_then(|f| f.active_label(filter_field))
        .map(|label| format!("filter by {label}…"))
        .or_else(|| input.placeholder.clone())
        .unwrap_or_else(|| "type RyeOS input…".to_string());

    let has_route_target =
        input.submits_to_route() && (text.starts_with('/') || route.has_target());
    let has_affordance_target = input.submit_affordance().is_some();
    StudioInputVm {
        cursor,
        route_label,
        placeholder,
        hint,
        submit_enabled: !text.trim().is_empty() && (has_route_target || has_affordance_target),
        completion,
        live_filter,
        text,
    }
}

/// The deep-watch header for a braid: one summary line built from the source's
/// `summary` (chain status + chain-wide usage totals, from chain_replay).
/// Returns `None` when the source carries no `summary` — any non-chain timeline —
/// so the header only appears where it means something.
fn timeline_summary_entry(response: &serde_json::Value) -> Option<StudioTimelineEntryVm> {
    let summary = response.get("summary")?;
    let status = summary.get("status").and_then(|v| v.as_str()).unwrap_or("");
    let input = summary.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let output = summary.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let cost = summary.get("spend_usd").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let turns = summary.get("turns").and_then(|v| v.as_i64()).unwrap_or(0);
    let primary = format!("{status} · ↑{input} ↓{output} · ${cost:.4} · {turns} turns");
    Some(StudioTimelineEntryVm::Line {
        primary,
        meta: None,
        tone: status_tone(status),
        action: None,
        secondary_action: None,
    })
}

/// The response a facet-backed view renders: the seat-fold value at `facet`,
/// resolved through the shared `@facet:` grammar (so a dotted path like
/// `selection.summary` reads the field within the `selection` facet). `None`
/// when the facet is unset — the view then falls back to its `source` fetch.
fn facet_backed_response(core: &StudioCore, facet: &str) -> Option<serde_json::Value> {
    let fold = core.seat.fold();
    let resolved = super::content::resolve_params(
        &serde_json::Value::String(format!("@facet:{facet}")),
        |key| fold.get(key).cloned(),
    );
    (!resolved.is_null()).then_some(resolved)
}

/// Map a thread/chain status to a tone (the same status→tone vocabulary the
/// list/detail tone blocks declare, in code here for the summary header). Matches
/// the typed [`ThreadStatus`] variants so a new status is a compile error here,
/// not a silently-neutral string.
fn status_tone(status: &str) -> StudioTone {
    use super::dto::ThreadStatus;
    match ThreadStatus::from_wire(status) {
        ThreadStatus::Running | ThreadStatus::Created => StudioTone::Accent,
        ThreadStatus::Failed | ThreadStatus::Killed | ThreadStatus::TimedOut => StudioTone::Danger,
        ThreadStatus::Cancelled => StudioTone::Warn,
        ThreadStatus::Completed | ThreadStatus::Continued => StudioTone::Good,
        ThreadStatus::Unknown => StudioTone::Neutral,
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
    // `submit: route` — render the seat route truthfully (the target the
    // next submit lands on). "continuing" only when the input declares
    // conversation targeting AND the targeted thread actually accepts OPERATOR
    // follow-up per the substrate (`execution.supports_operator_followup` from
    // the fetched projections — distrust only an explicit `false`, so a
    // just-launched thread not yet in the list still reads as continuing). A
    // route with a stray `thread` on a non-targeting or machine-only thread
    // (e.g. graph) is not a conversation. No keybinding copy here; an author
    // overrides the whole strip via `target_label`.
    match (&route.invoke, &route.thread) {
        (None, _) => "no target — surface declares no route".to_string(),
        (Some(_), Some(thread))
            if input.target.is_some()
                && core.thread_supports_operator_followup(thread) != Some(false) =>
        {
            format!("→ continuing {thread}")
        }
        (Some(InvokeTemplate::Service { .. }), _) => "→ new conversation".to_string(),
        (Some(InvokeTemplate::Command { tokens }), _) => format!("→ /{} (new)", tokens.join(" ")),
        (Some(InvokeTemplate::UiFacet { key }), _) => format!("→ {key}"),
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
    view_ref: &str,
    input: &super::content::InputBlock,
    text: &str,
    cursor: usize,
) -> Vec<String> {
    // Inline @-mention: when the cursor is in an @-token, the hint lists
    // matching refs from the declared mentions source.
    if let Some(mentions) = input.mentions.as_ref() {
        if super::tokenize::active_mention(text, cursor).is_some() {
            let records = core
                .data
                .sources
                .get(&super::content::mention_source_key(view_ref, &input.id))
                .map(|response| super::content::project_mentions(mentions, response))
                .unwrap_or_default();
            return super::tokenize::mention_hint(&records, text, cursor)
                .into_iter()
                .collect();
        }
    }
    let Some(completion) = input.completion.as_ref() else {
        return Vec::new();
    };
    // The completion grammar is fetched through the generic keyed source path
    // (identical to mentions), keyed per (view_ref, input.id).
    let Some(response) = core
        .data
        .sources
        .get(&super::content::completion_source_key(view_ref, &input.id))
    else {
        return Vec::new();
    };
    let records = super::content::completion_records(completion, response);
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
                .map(|tile| tile_title(core, &tile.view))
                .unwrap_or_else(|| "Missing".to_string());
            let input = core.workspace.tiles.get(tile_id).and_then(|tile| {
                instance_input_vm(core, &tile_id.0.to_string(), &tile.view.view_ref)
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
    // Every tile is a bound view; the scene widgets (graph/atlas) dispatch
    // by `widget` inside `bound_view_vm`.
    bound_view_vm(core, tile_id, &tile.view.view_ref)
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

/// Launchable views: every view embedded in the effective surface,
/// graph/atlas included (they are ordinary `view:` items now). Nothing
/// here names a product concept — labels and hints come from the items.
pub(crate) fn launcher_items(core: &StudioCore) -> Vec<StudioLauncherItemVm> {
    let mut items: Vec<StudioLauncherItemVm> = Vec::new();
    for (view_ref, binding) in &core.views {
        let view = ViewSpec {
            view_ref: view_ref.clone(),
        };
        items.push(StudioLauncherItemVm {
            // Prefer the view item's authored name; fall back to the ref
            // (sans `view:` prefix) so unnamed views still identify.
            label: binding
                .name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    view_ref
                        .strip_prefix("view:")
                        .unwrap_or(view_ref)
                        .to_string()
                }),
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

/// Launcher items for the focused table row's affordances OTHER than its
/// activate (Enter) action — so row management (Cancel, …) is reachable. Each is
/// rebuilt as an `InvokeAffordance` from the row's raw record and the view's
/// declared affordances. Table lenses only (the list surfaces are all tables);
/// rows-widget affordances are out of scope here.
fn focused_row_affordance_items(core: &StudioCore) -> Vec<StudioLauncherItemVm> {
    let Some(view) = core.workspace.focused_view() else {
        return Vec::new();
    };
    let view_ref = view.view_ref.clone();
    let Some(binding) = core.views.get(&view_ref) else {
        return Vec::new();
    };
    let Some(row) = focused_selected_table_row(core) else {
        return Vec::new();
    };
    if row.raw.is_null() {
        return Vec::new();
    }
    let activate = binding.selection.as_ref().map(|s| s.activate.as_str());
    binding
        .affordances
        .iter()
        .filter_map(|aff| {
            let id = aff.get("id").and_then(|v| v.as_str())?;
            if Some(id) == activate {
                return None; // already the row's Enter action
            }
            let label = aff
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or(id)
                .to_string();
            Some(StudioLauncherItemVm {
                label,
                hint: "focused row".to_string(),
                action: StudioAction::InvokeAffordance {
                    view_ref: view_ref.clone(),
                    affordance_id: id.to_string(),
                    record: row.raw.clone(),
                },
                secondary_action: None,
                enabled: true,
            })
        })
        .collect()
}

fn context_launcher_items(core: &StudioCore) -> Vec<StudioLauncherItemVm> {
    let mut items = Vec::new();

    // A recoverable failed terminal offers retry: re-submit its own stimulus as
    // a continuation, retargeted at the selected failed thread, pre-filled for
    // review (not one-click). Surfaced two ways so it works everywhere — as the
    // Inspect item's Shift+Enter secondary, AND as a distinct plain-Enter item
    // (clients that can't send Shift+Enter still reach it).
    let retry = retry_action_for_focused_row(core);

    if let Some(action) = inspect_action_for_focused_row(core) {
        let hint = if retry.is_some() {
            "Enter inspect · Shift+Enter retry".to_string()
        } else {
            focused_selection_hint(core).unwrap_or_else(|| "focused row".to_string())
        };
        items.push(StudioLauncherItemVm {
            label: "Inspect selection".to_string(),
            hint,
            action,
            secondary_action: retry.clone(),
            enabled: true,
        });
    }

    if let Some(action) = retry {
        items.push(StudioLauncherItemVm {
            label: "Retry failed turn".to_string(),
            hint: "re-submit this failed turn (review, then Enter)".to_string(),
            action,
            secondary_action: None,
            enabled: true,
        });
    }

    // The focused row's non-activate affordances (e.g. Cancel on a thread row),
    // so row management is reachable — the row's Enter action is only the
    // activate affordance.
    items.extend(focused_row_affordance_items(core));

    // Steering the active execution: offered only when the route has a head
    // thread. Each dispatches the shared SubmitThreadCommand → commands/submit.
    if let Some(head) = core.seat.fold().input_route().thread {
        // "continue" is an operator follow-up — gate it on the substrate fact so
        // a machine-only thread (graph) doesn't offer an operator continue the
        // daemon refuses. "cancel" (terminate) applies to any active thread.
        //
        // No command-style "interrupt" item: the operator interrupts a running
        // directive by submitting text with Alt+Enter (a live cognition_in
        // redirect via threads/input) — "Interrupt" is reserved for that. The old
        // commands/submit "interrupt" was inert for directives (nothing claims it)
        // and only muddied the meaning.
        use crate::studio::dto::ThreadControlCommand;
        let operator_continuable = core.thread_supports_operator_followup(&head) != Some(false);
        for (label, command) in [
            ("Continue thread", ThreadControlCommand::Continue),
            ("Cancel thread", ThreadControlCommand::Cancel),
        ] {
            items.push(StudioLauncherItemVm {
                label: label.to_string(),
                hint: "active thread".to_string(),
                action: StudioAction::SubmitThreadCommand { command },
                secondary_action: None,
                enabled: command != ThreadControlCommand::Continue || operator_continuable,
            });
        }
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
    if let Some(row) = focused_selected_row(core) {
        return Some(row.secondary.or(row.meta).unwrap_or(row.primary));
    }
    // Table lens: the first cell is the row's identifier.
    focused_selected_table_row(core).and_then(|row| row.cells.into_iter().next())
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

/// The keys overlay catalogue — the canonical global bindings, mirroring
/// `studio_key_command`. Static content (no per-state filtering): the overlay
/// is for discovery, so it always lists the full vocabulary.
fn help(core: &StudioCore) -> StudioHelpVm {
    let entry = |category: &str, keys: &str, description: &str| StudioHelpEntryVm {
        category: category.to_string(),
        keys: keys.to_string(),
        description: description.to_string(),
    };
    StudioHelpVm {
        open: core.ui.help_open,
        entries: vec![
            entry("Move", "↑ / ↓", "Move the point through rows; else move focus"),
            entry("Move", "← / →", "Fold / unfold the section under the point; else move focus"),
            entry("Act", "Enter", "Activate the selected row (or steer-submit when typing)"),
            entry("Act", "Alt+Enter", "Submit as an interrupt — cut the running thread's turn and redirect"),
            entry("Act", "Tab / ⇧Tab", "Accept completion, else cycle the route target"),
            entry("Act", "Esc", "Cancel a running thread; else close the lens"),
            entry("Input", "type", "The foot input is always live — text routes at the directive"),
            entry("Lenses", "Ctrl+K", "Open the lens launcher (swap the center lens)"),
            entry("Lenses", "Ctrl+← / →", "Switch workspace tab"),
            entry("Layout", "Ctrl+↑ / ↓", "Move the focused tile in the stack"),
            entry("Layout", "Ctrl+⇧+arrows", "Resize the focused tile"),
            entry("Layout", "Alt+M", "Toggle the focused tile master / full"),
            entry("Layout", "Alt+T / Alt+B", "Toggle the top / bottom status bar"),
            entry("App", "Alt+Q", "Close the focused lens"),
            entry("App", "Ctrl+C", "Quit"),
            entry("App", "?", "Show / hide this help"),
        ],
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
    // Feed lens: activation acts on the entry under the point (e.g. enter a
    // forked subthread, inspect an error terminal), not a row.
    if let Some(entry) = focused_timeline_entry(core) {
        return match entry {
            StudioTimelineEntryVm::Line { action, .. } => action,
            _ => None,
        };
    }
    if let Some(action) = focused_selected_row(core).and_then(|row| row.action) {
        return Some(action);
    }
    // Table lens: rows carry the same activation affordance, on a distinct VM.
    focused_selected_table_row(core).and_then(|row| row.action)
}

/// The timeline entry under the point in the focused feed lens, if the focused
/// view is a timeline with a point on an entry. The single home for reading the
/// focused feed entry — both the Enter action and the launcher-surfaced
/// secondary (retry) derive from it.
fn focused_timeline_entry(core: &StudioCore) -> Option<StudioTimelineEntryVm> {
    let tile_id = core.workspace.focused_tile;
    let view = core.workspace.focused_view()?;
    if let StudioViewVm::Timeline {
        entries, selected, ..
    } = bound_view_vm(core, tile_id, &view.view_ref)
    {
        return selected.and_then(|i| entries.into_iter().nth(i));
    }
    None
}

/// The focused feed entry's secondary affordance — the retry a recoverable
/// failed terminal carries. Surfaced through the launcher (its Shift+Enter
/// secondary and a distinct "Retry failed turn" item), never a direct feed key,
/// so Enter stays inspect.
fn retry_action_for_focused_row(core: &StudioCore) -> Option<StudioAction> {
    match focused_timeline_entry(core)? {
        StudioTimelineEntryVm::Line {
            secondary_action, ..
        } => secondary_action,
        _ => None,
    }
}

/// The row under the point in the focused tile, if the point is on a row. The
/// rows widget indexes by the flat cursor; sections carry the selection on the
/// row VM itself (the point may instead be on a collapsed header → no row).
/// Scene widgets (graph/atlas) have no rows.
fn focused_selected_row(core: &StudioCore) -> Option<StudioRowVm> {
    let tile_id = core.workspace.focused_tile;
    let view = core.workspace.focused_view()?;
    match bound_view_vm(core, tile_id, &view.view_ref) {
        StudioViewVm::Rows { rows, .. } => {
            let cursor = selected_cursor(core, tile_id).unwrap_or(0);
            rows.into_iter().nth(cursor)
        }
        StudioViewVm::Sections { sections, .. } => sections
            .into_iter()
            .flat_map(|section| section.rows)
            .find(|row| row.selected),
        _ => None,
    }
}

/// The table row under the point in the focused tile. Table rows are a distinct
/// VM (`StudioTableRowVm`, columnar cells) from the rows widget, so they need
/// their own selection projection — same flat cursor, different shape.
fn focused_selected_table_row(core: &StudioCore) -> Option<StudioTableRowVm> {
    let tile_id = core.workspace.focused_tile;
    let view = core.workspace.focused_view()?;
    match bound_view_vm(core, tile_id, &view.view_ref) {
        StudioViewVm::Table { rows, .. } => {
            let cursor = selected_cursor(core, tile_id).unwrap_or(0);
            rows.into_iter().nth(cursor)
        }
        _ => None,
    }
}

fn selected_cursor(core: &StudioCore, tile_id: TileId) -> Option<usize> {
    let tile = core.workspace.tiles.get(&tile_id)?;
    match &tile.local {
        ViewLocalState::GenericList { cursor, .. } => Some(*cursor),
        ViewLocalState::None => None,
    }
}

fn selected_collapsed(
    core: &StudioCore,
    tile_id: TileId,
) -> Option<&std::collections::BTreeSet<usize>> {
    match &core.workspace.tiles.get(&tile_id)?.local {
        ViewLocalState::GenericList { collapsed, .. } => Some(collapsed),
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
    use crate::studio::content::{ProjectedRecord, TimelineRole};
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

    fn session_with_views(views: serde_json::Value, tiles: serde_json::Value) -> StudioCore {
        let session = crate::studio::model::BrowserSession {
            session_id: "S-title".to_string(),
            surface_ref: "surface:ryeos/studio/base".to_string(),
            effective_surface: Some(json!({
                "name": "studio-base",
                "tiles": tiles,
                "views": views,
            })),
            ..Default::default()
        };
        StudioCore::new(session, crate::studio::model::BrowserViewport::default(), 0)
    }

    #[test]
    fn tile_title_prefers_authored_name_over_ref_tail() {
        let core = session_with_views(
            json!({ "view:ryeos/graph/topology": { "widget": "graph", "name": "Topology" } }),
            json!(["view:ryeos/graph/topology"]),
        );
        // Authored name wins over the ref tail ("topology").
        assert_eq!(focused_tile_title(&core), "Topology");
    }

    #[test]
    fn tile_title_falls_back_to_ref_tail_when_name_absent_or_blank() {
        let absent = session_with_views(
            json!({ "view:ryeos/graph/topology": { "widget": "graph" } }),
            json!(["view:ryeos/graph/topology"]),
        );
        assert_eq!(focused_tile_title(&absent), "topology");

        let blank = session_with_views(
            json!({ "view:ryeos/graph/topology": { "widget": "graph", "name": "  " } }),
            json!(["view:ryeos/graph/topology"]),
        );
        assert_eq!(focused_tile_title(&blank), "topology");
    }

    #[test]
    fn launcher_label_prefers_authored_name_else_stripped_ref() {
        let core = session_with_views(
            json!({
                "view:ryeos/atlas": { "widget": "atlas", "name": "Atlas", "description": "the namespace atlas" },
                "view:ryeos/x/raw": { "widget": "rows" },
            }),
            json!([]),
        );
        let items = launcher_items(&core);
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert!(
            labels.contains(&"Atlas"),
            "named view uses its name: {labels:?}"
        );
        assert!(
            labels.contains(&"ryeos/x/raw"),
            "unnamed view falls back to stripped ref: {labels:?}"
        );
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
                    action: None,
                    secondary_action: None,
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
                action: None,
                secondary_action: None,
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
                action: None,
                secondary_action: None,
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
    fn actual_threads_list_binding_projects_a_table() {
        use crate::studio::content::{project_table, table_columns, ViewBinding};
        let binding: ViewBinding = serde_yaml::from_str(include_str!(
            "../../../../../bundles/studio/.ai/views/ryeos/threads/list.yaml"
        ))
        .unwrap();
        assert_eq!(binding.widget, "table");
        // The watch dashboard sources the operator-scoped UI-studio list,
        // active-first (sort: watch), before the limit.
        let source = binding.source.as_ref().expect("threads list has a source");
        assert_eq!(source.item_ref, "service:ui/studio/threads/list");
        assert_eq!(source.params["limit"], 200);
        assert_eq!(source.params["sort"], "watch");
        assert_eq!(source.collection.as_deref(), Some("threads"));
        let columns = table_columns(&binding);
        assert_eq!(
            columns.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
            ["thread", "kind", "item", "status", "source", "created"]
        );
        let rows = project_table(
            &binding,
            &json!({ "threads": [
                { "thread_id": "T-ab", "kind": "directive", "item_ref": "directive:ops/base", "status": "running", "requested_by": "fp:claude", "created_at": "2026-06-29T00:00:00Z" },
                { "thread_id": "T-cd", "kind": "graph", "item_ref": "directive:ops/scan", "status": "failed",  "requested_by": "fp:amp", "created_at": "2026-06-28T00:00:00Z" }
            ]}),
            &columns,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].cells,
            ["T-ab", "directive", "directive:ops/base", "running", "fp:claude", "2026-06-29T00:00:00Z"]
        );
        // Tone reuses the shared status→tone block the rows widget would.
        assert_eq!(rows[0].tone.as_deref(), Some("accent"));
        assert_eq!(rows[1].tone.as_deref(), Some("danger"));
        // The raw record is preserved for affordance interpolation.
        assert_eq!(rows[0].raw["thread_id"], "T-ab");
    }

    #[test]
    fn timeline_summary_entry_builds_a_header_line_from_the_summary() {
        let response = json!({
            "events": [],
            "summary": {
                "status": "running",
                "input_tokens": 1200,
                "output_tokens": 340,
                "spend_usd": 0.0421,
                "turns": 3
            }
        });
        match timeline_summary_entry(&response).expect("summary present") {
            StudioTimelineEntryVm::Line { primary, tone, .. } => {
                assert!(primary.contains("running"), "{primary}");
                assert!(primary.contains("1200") && primary.contains("340"), "{primary}");
                assert!(primary.contains("$0.0421"), "{primary}");
                assert!(primary.contains("3 turns"), "{primary}");
                assert_eq!(tone, StudioTone::Accent);
            }
            other => panic!("expected a header Line, got {other:?}"),
        }
        // A timeline whose source carries no `summary` gets no header.
        assert!(timeline_summary_entry(&json!({ "events": [] })).is_none());
    }

    #[test]
    fn status_tone_maps_by_typed_status_variant() {
        assert_eq!(status_tone("running"), StudioTone::Accent);
        assert_eq!(status_tone("created"), StudioTone::Accent);
        assert_eq!(status_tone("completed"), StudioTone::Good);
        assert_eq!(status_tone("continued"), StudioTone::Good);
        assert_eq!(status_tone("failed"), StudioTone::Danger);
        assert_eq!(status_tone("killed"), StudioTone::Danger);
        assert_eq!(status_tone("timed_out"), StudioTone::Danger);
        assert_eq!(status_tone("cancelled"), StudioTone::Warn);
        // An unrecognized status folds to Unknown → neutral, not a panic.
        assert_eq!(status_tone("some_future_status"), StudioTone::Neutral);
    }

    #[test]
    fn actual_thread_detail_binding_projects_inspect_sections() {
        use crate::studio::content::{project_section, ViewBinding};
        let binding: ViewBinding = serde_yaml::from_str(include_str!(
            "../../../../../bundles/studio/.ai/views/ryeos/threads/detail.yaml"
        ))
        .unwrap();
        assert_eq!(binding.widget, "sections");
        // One inspect response feeds every section; each reads a different
        // sub-value of it (thread / result / artifacts / children).
        let response = json!({
            "schema_version": "studio.thread.inspect.v1",
            "thread": { "thread_id": "T-ab", "item_ref": "directive:ops/base", "status": "running" },
            "result": { "outcome_code": "error", "error": "boom" },
            "artifacts": [ { "artifact_type": "file", "uri": "file://out.txt" } ],
            "children": [ { "item_ref": "directive:ops/child", "status": "completed" } ],
            "usage": [
                { "label": "input tokens", "value": "1200" },
                { "label": "cost", "value": "$0.0421" }
            ]
        });
        let section = |title: &str| {
            binding
                .sections
                .iter()
                .find(|s| s.title == title)
                .unwrap_or_else(|| panic!("section {title}"))
        };

        // Summary: the whole `thread` sub-object → one row (item • status), toned.
        let summary = project_section(section("Summary"), &response);
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].primary, "directive:ops/base");
        assert_eq!(summary[0].meta.as_deref(), Some("running"));
        assert_eq!(summary[0].tone.as_deref(), Some("accent"));

        // Outcome: the `result` sub-object (outcome code + error), toned danger.
        let outcome = project_section(section("Outcome"), &response);
        assert_eq!(outcome.len(), 1);
        assert_eq!(outcome[0].primary, "error");
        assert_eq!(outcome[0].meta.as_deref(), Some("boom"));
        assert_eq!(outcome[0].tone.as_deref(), Some("danger"));

        // Artifacts + children are list sub-arrays: one row each.
        let artifacts = project_section(section("Artifacts"), &response);
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].primary, "file");
        let children = project_section(section("Children"), &response);
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].primary, "directive:ops/child");
        assert_eq!(children[0].meta.as_deref(), Some("completed"));

        // Usage: one row per labeled metric (value primary, label meta).
        let usage = project_section(section("Usage"), &response);
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0].primary, "1200");
        assert_eq!(usage[0].meta.as_deref(), Some("input tokens"));
        assert_eq!(usage[1].primary, "$0.0421");

        // Re-fetches on the selection facet; each section pulls the selected
        // thread via @facet, so an unset selection SUPPRESSES the fetch (D8)
        // rather than sending a null thread_id to the deny_unknown inspect svc.
        assert_eq!(
            binding.refresh.get("on_facet").and_then(|v| v.as_str()),
            Some("selection")
        );
        assert_eq!(
            section("Summary").source.params["thread_id"],
            "@facet:selection.thread"
        );
    }

    #[test]
    fn actual_items_space_binding_projects_renamed_and_nested_fields() {
        use crate::studio::content::{project_table, project_tone, table_columns, ViewBinding};
        let binding: ViewBinding = serde_yaml::from_str(include_str!(
            "../../../../../bundles/studio/.ai/views/ryeos/items/space.yaml"
        ))
        .unwrap();
        assert_eq!(binding.widget, "table");
        let columns = table_columns(&binding);
        assert_eq!(
            columns.iter().map(|c| c.field.as_str()).collect::<Vec<_>>(),
            ["canonical_ref", "item_kind", "space", "trust.class"]
        );
        let record = json!({
            "canonical_ref": "directive:ryeos/ops/base",
            "item_kind": "directive",
            "space": "system",
            "trust": { "class": "trusted" }
        });
        let rows = project_table(&binding, &json!({ "items": [record.clone()] }), &columns);
        assert_eq!(rows.len(), 1);
        // The real field is `item_kind` (not `kind`) and trust is a nested
        // object (`trust.class`, not a bare string) — both were silently dead
        // under the rows widget's projection; the table binding resolves them.
        assert_eq!(
            rows[0].cells,
            ["directive:ryeos/ops/base", "directive", "system", "trusted"]
        );
        // The nested tone path maps the same way.
        assert_eq!(rows[0].tone.as_deref(), Some("good"));
        assert_eq!(
            project_tone(&record, &binding.projections).as_deref(),
            Some("good")
        );
    }

    #[test]
    fn table_flat_cursor_selects_a_row_and_resolves_its_activation() {
        use crate::studio::event::{StudioEvent, StudioUiEvent};
        use crate::studio::model::{BrowserSession, BrowserViewport};
        let session = BrowserSession {
            effective_surface: Some(json!({
                "name": "t",
                "tiles": ["view:ryeos/threads/list"],
                "views": {
                    "view:ryeos/threads/list": {
                        "widget": "table",
                        "source": { "ref": "service:threads/list", "collection": "threads" },
                        "projections": {
                            "columns": [
                                { "label": "thread", "field": "thread_id" },
                                { "label": "status", "field": "status" }
                            ],
                            "tone": { "field": "status", "map": { "running": "accent" }, "default": "neutral" }
                        },
                        "selection": { "activate": "inspect" },
                        "affordances": [{
                            "id": "inspect",
                            "label": "Inspect",
                            "invoke": { "plane": "ui", "facet": "selection", "value": { "thread": "{record.thread_id}" } }
                        }]
                    }
                }
            })),
            read_only: false,
            ..Default::default()
        };
        let mut core = StudioCore::new(session, BrowserViewport::default(), 0);
        let key = core.workspace.focused_tile.0.to_string();
        core.data.sources.insert(
            key.clone(),
            json!({ "threads": [
                { "thread_id": "T-ab", "status": "running" },
                { "thread_id": "T-cd", "status": "running" }
            ]}),
        );

        fn find_tile_view(node: &StudioLayoutNodeVm) -> Option<&StudioViewVm> {
            match node {
                StudioLayoutNodeVm::Tile { view, .. } => Some(view),
                StudioLayoutNodeVm::Split { first, second, .. } => {
                    find_tile_view(first).or_else(|| find_tile_view(second))
                }
            }
        }
        let selected_cells = |core: &StudioCore| -> Vec<Vec<String>> {
            let vm = build_view_model(core);
            let root = vm.workspace.root.expect("layout root");
            match find_tile_view(&root).expect("tile view") {
                StudioViewVm::Table { rows, .. } => rows
                    .iter()
                    .filter(|r| r.selected)
                    .map(|r| r.cells.clone())
                    .collect(),
                other => panic!("expected table view, got {other:?}"),
            }
        };

        // Flat cursor 1 = the second row; activation carries that row's record.
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SetTileCursor { tile_id: key.clone(), index: 1 },
        });
        assert_eq!(
            selected_cells(&core),
            vec![vec!["T-cd".to_string(), "running".to_string()]]
        );
        match action_for_focused_row(&core).expect("table row activates") {
            StudioAction::InvokeAffordance { affordance_id, record, .. } => {
                assert_eq!(affordance_id, "inspect");
                assert_eq!(record["thread_id"], "T-cd");
            }
            other => panic!("expected inspect invoke, got {other:?}"),
        }
    }

    #[test]
    fn launcher_surfaces_focused_row_cancel_but_not_the_activate() {
        use crate::studio::model::{BrowserSession, BrowserViewport};
        let session = BrowserSession {
            effective_surface: Some(json!({
                "name": "t",
                "tiles": ["view:ryeos/threads/list"],
                "views": {
                    "view:ryeos/threads/list": {
                        "widget": "table",
                        "source": { "ref": "service:ui/studio/threads/list", "collection": "threads" },
                        "projections": { "columns": [ { "label": "thread", "field": "thread_id" } ] },
                        "selection": { "activate": "watch" },
                        "affordances": [
                            { "id": "watch", "label": "Watch",
                              "invoke": { "plane": "ui", "facet": "input.route",
                                          "merge": { "thread": "{record.thread_id}" },
                                          "open_view": "view:ryeos/chain/timeline" } },
                            { "id": "cancel", "label": "Cancel",
                              "invoke": { "plane": "rye", "ref": "service:ui/studio/thread/cancel",
                                          "args": { "thread_id": "{record.thread_id}" } } }
                        ]
                    }
                }
            })),
            read_only: false,
            ..Default::default()
        };
        let mut core = StudioCore::new(session, BrowserViewport::default(), 0);
        let key = core.workspace.focused_tile.0.to_string();
        core.data.sources.insert(
            key,
            json!({ "threads": [ { "thread_id": "T-ab", "chain_root_id": "T-ab" } ] }),
        );

        let items = launcher_items_for(&core);
        // The focused row (default cursor 0 = T-ab) exposes a reachable Cancel
        // targeting that specific row.
        let cancel = items
            .iter()
            .find(|i| i.label == "Cancel")
            .expect("cancel launcher item");
        assert!(
            matches!(&cancel.action,
                StudioAction::InvokeAffordance { view_ref, affordance_id, record }
                    if view_ref == "view:ryeos/threads/list"
                        && affordance_id == "cancel"
                        && record["thread_id"] == "T-ab"),
            "cancel item must invoke the row's cancel affordance; got {:?}",
            cancel.action
        );
        // The activate (watch) affordance is NOT duplicated as a context item —
        // it's already the row's Enter action.
        assert!(
            !items.iter().any(|i| i.label == "Watch"),
            "activate affordance should not be surfaced as a context item"
        );
    }

    /// Build a single focused timeline tile over a chain_replay response, with
    /// the feed point (distance-from-bottom 0) on the newest entry.
    fn feed_core(events: serde_json::Value) -> StudioCore {
        use crate::studio::model::{BrowserSession, BrowserViewport};
        let session = BrowserSession {
            effective_surface: Some(json!({
                "name": "t",
                "tiles": ["view:ryeos/chain/timeline"],
                "views": {
                    "view:ryeos/chain/timeline": {
                        "widget": "timeline",
                        "source": { "ref": "service:events/chain_replay", "collection": "events" }
                    }
                }
            })),
            read_only: false,
            ..Default::default()
        };
        let mut core = StudioCore::new(session, BrowserViewport::default(), 0);
        let key = core.workspace.focused_tile.0.to_string();
        core.data.sources.insert(key.clone(), json!({ "events": events }));
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SetTileCursor { tile_id: key, index: 0 },
        });
        core
    }

    #[test]
    fn launcher_offers_inspect_and_retry_on_a_focused_failed_feed_entry() {
        let core = feed_core(json!([
            { "event_type": "cognition_in", "thread_id": "T-1", "payload": { "content": "do it" } },
            { "event_type": "thread_failed", "thread_id": "T-1", "chain_root_id": "R-1",
              "payload": { "error": { "message": "boom" } } }
        ]));

        let items = launcher_items_for(&core);
        // Enter=inspect, carrying the visible line as title and the raw event.
        let inspect = items
            .iter()
            .find(|i| i.label == "Inspect selection")
            .expect("inspect item on a failed entry");
        assert!(
            matches!(&inspect.action, StudioAction::InspectSummary { title, .. } if title == "failed — boom")
        );
        // Retry is the inspect item's Shift+Enter secondary …
        assert!(
            matches!(&inspect.secondary_action,
                Some(StudioAction::PrefillRetryTurn { thread_id, chain_root_id, input })
                    if thread_id == "T-1" && chain_root_id == "R-1" && input == "do it"),
            "retry is offered as the inspect item's secondary; got {:?}",
            inspect.secondary_action
        );
        // … AND a distinct plain-Enter item (for clients that can't send Shift+Enter).
        let retry = items
            .iter()
            .find(|i| i.label == "Retry failed turn")
            .expect("distinct retry item");
        assert!(matches!(
            &retry.action,
            StudioAction::PrefillRetryTurn { thread_id, .. } if thread_id == "T-1"
        ));
    }

    #[test]
    fn launcher_offers_neither_inspect_nor_retry_on_a_cancelled_terminal() {
        // Cancelled is operator-initiated, not an error — it is neither
        // inspectable nor retryable.
        let core = feed_core(json!([
            { "event_type": "cognition_in", "thread_id": "T-1", "payload": { "content": "do it" } },
            { "event_type": "thread_cancelled", "thread_id": "T-1", "chain_root_id": "R-1",
              "payload": {} }
        ]));
        let items = launcher_items_for(&core);
        assert!(!items.iter().any(|i| i.label == "Retry failed turn"));
        assert!(!items.iter().any(|i| i.label == "Inspect selection"));
    }

    #[test]
    fn launcher_offers_inspect_but_not_retry_on_a_timed_out_terminal() {
        // timed_out is inspectable but not retryable in v1 (the daemon refuses
        // continuation for that status).
        let core = feed_core(json!([
            { "event_type": "cognition_in", "thread_id": "T-1", "payload": { "content": "do it" } },
            { "event_type": "thread_timed_out", "thread_id": "T-1", "chain_root_id": "R-1",
              "payload": {} }
        ]));
        let items = launcher_items_for(&core);
        assert!(items.iter().any(|i| i.label == "Inspect selection"));
        assert!(!items.iter().any(|i| i.label == "Retry failed turn"));
    }

    #[test]
    fn append_live_delta_adds_trailing_cursor_block_for_head_thread() {
        let mut core = StudioCore::default();
        core.seat.append_facet(
            crate::studio::seat::KEY_INPUT_ROUTE,
            json!({ "thread": "T-1" }),
        );
        core.data.live_delta = Some(crate::studio::model::StudioLiveDelta {
            thread: "T-1".to_string(),
            text: "Hel".to_string(),
        });

        let mut entries = Vec::new();
        append_live_delta(&core, &mut entries);

        // Accent-toned trailing block with a cursor — the in-progress turn.
        assert!(matches!(
            entries.as_slice(),
            [StudioTimelineEntryVm::Block { text, tone: StudioTone::Accent }]
                if text == "Hel\u{258d}"
        ));
    }

    #[test]
    fn append_live_delta_ignores_buffer_for_non_head_thread() {
        let mut core = StudioCore::default();
        core.seat.append_facet(
            crate::studio::seat::KEY_INPUT_ROUTE,
            json!({ "thread": "T-1" }),
        );
        // A buffer left over from a different head must not render.
        core.data.live_delta = Some(crate::studio::model::StudioLiveDelta {
            thread: "T-OTHER".to_string(),
            text: "stale".to_string(),
        });

        let mut entries = Vec::new();
        append_live_delta(&core, &mut entries);
        assert!(entries.is_empty());
    }

    #[test]
    fn append_live_delta_shows_working_indicator_when_head_runs_silently() {
        let mut core = StudioCore::default();
        core.seat
            .append_facet(crate::studio::seat::KEY_INPUT_ROUTE, json!({ "thread": "T-1" }));
        // Head thread is running but has emitted no streaming text yet.
        core.data.threads = Some(crate::studio::dto::StudioThreadsDto {
            threads: vec![json!({ "thread_id": "T-1", "status": "running" })],
        });

        let mut entries = Vec::new();
        append_live_delta(&core, &mut entries);
        assert!(
            matches!(
                entries.as_slice(),
                [StudioTimelineEntryVm::Line { primary, tone: StudioTone::Accent, .. }]
                    if primary.contains("working")
            ),
            "running head with no tail → working indicator: {entries:?}"
        );
    }

    #[test]
    fn append_live_delta_no_indicator_when_head_thread_settled() {
        let mut core = StudioCore::default();
        core.seat
            .append_facet(crate::studio::seat::KEY_INPUT_ROUTE, json!({ "thread": "T-1" }));
        core.data.threads = Some(crate::studio::dto::StudioThreadsDto {
            threads: vec![json!({ "thread_id": "T-1", "status": "completed" })],
        });

        let mut entries = Vec::new();
        append_live_delta(&core, &mut entries);
        assert!(entries.is_empty(), "settled head → no working indicator");
    }

    #[test]
    fn conversation_usage_sums_thread_usage_across_braid_sources() {
        let mut core = StudioCore::default();
        core.data.sources.insert(
            "timeline".to_string(),
            json!({ "events": [
                { "event_type": "thread_usage", "payload": { "input_tokens": 100, "output_tokens": 20 } },
                { "event_type": "cognition_out", "payload": { "content": "hi" } },
                { "event_type": "thread_usage", "payload": { "input_tokens": 5, "output_tokens": 3 } },
            ]}),
        );
        assert_eq!(conversation_usage(&core), (105, 23));
    }

    #[test]
    fn conversation_usage_is_zero_without_usage_events() {
        assert_eq!(conversation_usage(&StudioCore::default()), (0, 0));
    }

    #[test]
    fn compact_count_abbreviates_thousands() {
        assert_eq!(compact_count(0), "0");
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(1234), "1.2k");
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
        core.data.sources.insert(
            crate::studio::content::completion_source_key("view:ryeos/input", "line"),
            json!({
                "commands": [
                    { "invocable": true, "tokens": ["thread", "list"], "description": "List threads" },
                    { "invocable": true, "tokens": ["thread", "get"], "description": "Get thread" }
                ]
            }),
        );
        core.ui.input_buffers.insert(
            crate::studio::model::InputBufferKey::new("dock:bottom", "view:ryeos/input", "line")
                .storage_key(),
            crate::studio::model::StudioInputState {
                text: "/thread ".to_string(),
                cursor: "/thread ".len(),
                ..Default::default()
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
        core.data.sources.insert(
            crate::studio::content::completion_source_key("view:ryeos/input", "line"),
            json!({ "commands": [
                { "invocable": true, "tokens": ["thread", "list"] }
            ] }),
        );
        core.ui.input_buffers.insert(
            crate::studio::model::InputBufferKey::new("dock:bottom", "view:ryeos/input", "line")
                .storage_key(),
            crate::studio::model::StudioInputState {
                text: "/thr".to_string(),
                cursor: 4,
                ..Default::default()
            },
        );
        let vm = build_view_model(&core);
        let input = vm.workspace.docks.bottom.unwrap().input.unwrap();
        assert!(
            input.completion.is_empty(),
            "no completion source -> no suggestions"
        );
    }

    #[test]
    fn completion_shows_mention_refs_inside_an_at_token() {
        let mut core = input_session(json!({
            "widget": "text",
            "input": {
                "id": "line",
                "submit": "route",
                "mentions": {
                    "ref": "service:threads/list",
                    "collection": "threads",
                    "reference": "thread_id",
                    "label": "item_ref"
                }
            }
        }));
        core.data.sources.insert(
            crate::studio::content::mention_source_key("view:ryeos/input", "line"),
            json!({ "threads": [
                { "thread_id": "T-ab", "item_ref": "directive:ops/base" },
                { "thread_id": "T-cd", "item_ref": "directive:demo/chat" }
            ]}),
        );
        core.ui.input_buffers.insert(
            crate::studio::model::InputBufferKey::new("dock:bottom", "view:ryeos/input", "line")
                .storage_key(),
            crate::studio::model::StudioInputState {
                text: "ping @directive".to_string(),
                cursor: "ping @directive".len(),
                ..Default::default()
            },
        );
        let vm = build_view_model(&core);
        let input = vm.workspace.docks.bottom.unwrap().input.unwrap();
        assert!(
            input
                .completion
                .iter()
                .any(|s| s.contains("directive:ops/base") && s.contains("directive:demo/chat")),
            "mention hint lists matching refs by label: {:?}",
            input.completion
        );
    }
}
