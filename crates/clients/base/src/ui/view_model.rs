use serde::{Deserialize, Serialize};

use super::content::ViewBinding;
use super::event::RyeOsUiIntent;
use super::model::{RyeOsCore, RyeOsDockContent, RyeOsDockEdge, RyeOsDockSlotState};
use super::scene_model::{build_scene_model, RyeOsSceneModel};
use super::seat::InvokeTemplate;
use crate::ids::TileId;
use crate::layout::{LayoutTree, SplitAxis};
use crate::surface::{AmbientAtlasStyleSpec, SurfaceSpec};
use crate::workspace::{TileState, ViewLocalState, ViewSpec};

mod dialogs;
mod navigation;
pub use dialogs::{
    RyeOsOverlayChoice, RyeOsOverlayItemVm, RyeOsOverlayVm, RyeOsShortcutEntryVm,
    RyeOsTileIntentVm,
};
pub use navigation::{
    RyeOsAmbientAtlasStyleVm, RyeOsAmbientAtlasVm, RyeOsAmbientModeVm, RyeOsAmbientVm,
    RyeOsSessionVm,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsViewModel {
    pub schema_version: String,
    pub generation: u64,
    #[serde(default)]
    pub now_ms: u64,
    pub session: RyeOsSessionVm,
    pub chrome: RyeOsChromeVm,
    pub presentation: RyeOsPresentationVm,
    pub workspace: RyeOsWorkspaceVm,
    pub overlays: Vec<RyeOsOverlayVm>,
    pub notices: Vec<RyeOsNoticeVm>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsChromeVm {
    pub title: String,
    pub subtitle: String,
    pub health_label: String,
    pub health_tone: RyeOsTone,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsPresentationVm {
    pub schema_version: String,
    pub theme: RyeOsThemeVm,
    pub chrome: RyeOsPresentationChromeVm,
    pub metrics: RyeOsPresentationMetricsVm,
    pub frame: RyeOsFrameVm,
    pub motion: Vec<RyeOsMotionEventVm>,
}

/// Shared semantic presentation signals.
///
/// Rust owns RyeOS meaning: counts, modes, health, focus, and semantic motion.
/// Renderers own pixels, easing, glyph choice, DOM/canvas/TUI implementation,
/// and how these signals are mapped into local visual affordances.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsPresentationMetricsVm {
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
pub struct RyeOsThemeVm {
    pub id: String,
    pub tone: RyeOsTone,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsPresentationChromeVm {
    pub title: String,
    pub version_label: String,
    /// Surface-declared border treatment for tiles, dock tiles, and
    /// panels: thick | thin | hidden | none. Renderers map the name to
    /// local glyphs/pixels — content declares, renderers map.
    pub border: String,
    pub top_bar: RyeOsTopBarVm,
    pub status_bar: RyeOsStatusBarVm,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsTopBarVm {
    pub visible: bool,
    pub tabs: Vec<RyeOsWorkspaceTabVm>,
    pub focused_title: String,
    pub layout_symbol: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsWorkspaceTabVm {
    pub number: usize,
    pub active: bool,
    pub tile_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsStatusBarVm {
    pub visible: bool,
    pub segments: Vec<RyeOsStatusSegmentVm>,
    pub key_hint: String,
    #[serde(default)]
    pub energy: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<RyeOsTone>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsStatusSegmentVm {
    pub id: String,
    pub label: Option<String>,
    pub value: String,
    pub tone: RyeOsTone,
    pub grow: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsFrameVm {
    pub corners: RyeOsFrameCornersVm,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsFrameCornersVm {
    pub visible: bool,
    pub tone: RyeOsTone,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RyeOsMotionEventVm {
    TileEnter {
        tile_id: String,
    },
    TileExit {
        tile_id: String,
    },
    TileSplit {
        source_tile_id: String,
        new_tile_id: String,
        axis: RyeOsSplitAxisVm,
    },
    FocusChanged {
        tile_id: String,
    },
    TabChanged {
        workspace_number: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsWorkspaceVm {
    /// The computed layout tree. None when the center is empty (the
    /// backdrop scene shows) — the engine computes this from the ordered
    /// tile list and the tiling algorithm; it is never authored.
    pub root: Option<RyeOsLayoutNodeVm>,
    pub focused_tile: String,
    /// True when the center has no tiles. Drives backdrop-vs-tiles: when
    /// empty, the renderer draws `backdrop`; otherwise the layout tree.
    pub center_is_empty: bool,
    /// The resolved backdrop scene, present only when `center_is_empty`
    /// and the surface declares a `backdrop` view. A normal
    /// `RyeOsSceneModel` the generic scene renderer draws — the
    /// background is content, never a renderer enum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backdrop: Option<RyeOsSceneModel>,
    pub tile_count: usize,
    #[serde(default)]
    pub docks: RyeOsDockPlaneVm,
    /// Labels of the levels on the step-in return stack, root-first (the
    /// deepest drill last). Empty at the top of the tree. Renderers draw it as
    /// a breadcrumb — `threads ▸ ar25 ▸ …` — so the operator sees how deep the
    /// execution drill is and that Backspace returns. Each is the level's own
    /// label (the cognition/thread it showed) when known, else the view title.
    /// The current (focused) level is NOT included; it is `lens_label` (or the
    /// focused view's title) as the tail beyond this trail.
    #[serde(default)]
    pub lens_trail: Vec<String>,
    /// Label of the CURRENT focused level (the cognition stepped into, e.g.
    /// `study`), the breadcrumb tail. `None` at the top of the tree, where the
    /// focused view's own title stands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lens_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RyeOsDockPlaneVm {
    pub top: Option<RyeOsDockTileVm>,
    pub bottom: Option<RyeOsDockTileVm>,
    pub left: Option<RyeOsDockTileVm>,
    pub right: Option<RyeOsDockTileVm>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsDockTileVm {
    pub edge: RyeOsDockEdge,
    pub title: String,
    pub size: u16,
    #[serde(default)]
    pub focused: bool,
    /// The bound view (every slot is a view instance; input is no longer a
    /// dock-content variant).
    pub view: RyeOsViewVm,
    /// Present when this instance declares an `input` block: the prompt
    /// renderers draw (target strip, buffer, cursor, completion). Any
    /// widget may carry a prompt — input is an orthogonal capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<RyeOsInputVm>,
}

/// The projection of a view instance's active input buffer. Shape
/// preserved from the deleted Input dock variant; re-sourced from the
/// instance's transient buffer rather than a dock content variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsInputVm {
    pub text: String,
    pub cursor: usize,
    #[serde(default)]
    pub focused: bool,
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
pub enum RyeOsLayoutNodeVm {
    Split {
        axis: RyeOsSplitAxisVm,
        ratio: f32,
        first: Box<RyeOsLayoutNodeVm>,
        second: Box<RyeOsLayoutNodeVm>,
    },
    Tile {
        tile_id: String,
        focused: bool,
        title: String,
        intents: Vec<RyeOsTileIntentVm>,
        view: RyeOsViewVm,
        #[serde(default)]
        chrome_hidden: bool,
        #[serde(default)]
        background_transparent: bool,
        /// Present when the tile's view declares an `input` block.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<RyeOsInputVm>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsSplitAxisVm {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RyeOsViewVm {
    Text {
        title: String,
        lines: Vec<RyeOsTextLineVm>,
        position: RyeOsTextPositionVm,
    },
    /// The generic content widget surface: every bound view renders
    /// through rows (typed widget variants arrive with the render pass).
    Rows {
        title: String,
        columns: Vec<String>,
        /// Total selectable rows in the source collection. `rows` may be a
        /// render window around the cursor for large lists.
        #[serde(default)]
        total_rows: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provenance: Option<String>,
        #[serde(default)]
        affordance_hints: Vec<String>,
        rows: Vec<RyeOsRowVm>,
    },
    Timeline {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provenance: Option<String>,
        #[serde(default)]
        affordance_hints: Vec<String>,
        entries: Vec<RyeOsTimelineEntryVm>,
        /// Call-tree indent depth per entry (parallel to `entries`): a graph
        /// node's tool calls and its directive/sub-graph fork nest one level
        /// under the node. Empty or shorter than `entries` renders flat (depth
        /// 0) — the renderer never trusts the index.
        #[serde(default)]
        entry_indents: Vec<u8>,
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
        /// Whether each visible entry can be expanded in place (parallel to
        /// `entries`). Timeline expansion uses the same local state as row
        /// expansion, keyed by the source event's stable identity.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        entry_expandable: Vec<bool>,
        /// Whether each visible entry is currently expanded (parallel to
        /// `entries`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        entry_expanded: Vec<bool>,
        /// Detail rows for each visible entry (parallel to `entries`). Empty
        /// vectors mean either collapsed or not expandable.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        entry_details: Vec<Vec<RyeOsRowDetailVm>>,
    },
    Map {
        scene: RyeOsSceneModel,
    },
    Atlas {
        scene: RyeOsSceneModel,
    },
    /// A foldable multi-section list — the magit-style status surface: one
    /// widget over many datasets, each section a titled, collapsible group of
    /// rows. The engine knows the `sections` widget vocabulary; the specific
    /// sections (threads/bundles/node/…) are declared by the bound view, never
    /// named here (no fire-sword). Rows reuse `RyeOsRowVm`, so per-row intents
    /// come for free.
    Sections {
        title: String,
        sections: Vec<RyeOsSectionVm>,
        /// The section the point sits in — what a fold key toggles. `None`
        /// when there is no point (empty view).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fold_section: Option<usize>,
    },
    /// A real column table — the typed list surface for the non-chat lenses
    /// (threads/bundles/schedules/…): aligned cells under headers rather than
    /// the rows widget's primary/secondary/meta. The engine knows the `table`
    /// widget vocabulary; the columns + their field projections are declared by
    /// the bound view (no fire-sword). Rows carry per-row intents like the rows
    /// widget.
    Table {
        title: String,
        columns: Vec<String>,
        /// Total selectable rows in the source collection. `rows` may be a
        /// render window around the cursor for large lists.
        #[serde(default)]
        total_rows: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provenance: Option<String>,
        #[serde(default)]
        affordance_hints: Vec<String>,
        rows: Vec<RyeOsTableRowVm>,
    },
    Placeholder {
        title: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsTextLineVm {
    pub text: String,
    pub tone: RyeOsTone,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RyeOsTextPositionVm {
    pub x: f32,
    pub y: f32,
}

/// One row of a `Table` view: a cell per declared column, plus the per-row
/// tone/intent/selection the rows widget also carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsTableRowVm {
    pub id: String,
    pub cells: Vec<String>,
    /// Per-cell tone overrides, parallel to `cells` (`None` = inherit the row
    /// tone). Present only when at least one column declares a `tone` block,
    /// so tables without per-column tones pay nothing on the wire.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cell_tones: Vec<Option<RyeOsTone>>,
    pub tone: RyeOsTone,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<RyeOsUiIntent>,
    #[serde(default)]
    pub selected: bool,
    #[serde(default)]
    pub expandable: bool,
    #[serde(default)]
    pub expanded: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub detail: Vec<RyeOsRowDetailVm>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_at_ms: Option<u64>,
    /// The row's raw record — base-only (not serialized to clients). Overlays
    /// rebuilds the row's non-activate affordances (e.g. Cancel) from it, so row
    /// management is reachable. Skipped to avoid duplicating every record into
    /// the per-row wire payload.
    #[serde(skip)]
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsRowDetailVm {
    pub field: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<RyeOsTone>,
}

/// One titled, collapsible group within a `Sections` view. `count` is the
/// section's total even when `collapsed` hides the rows, so the header can
/// report it without the rows being present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsSectionVm {
    pub title: String,
    pub count: usize,
    #[serde(default)]
    pub collapsed: bool,
    /// The point is on this section's header. Only a collapsed section's
    /// header is a point (it has no visible rows to land on); the highlight
    /// lets the operator see what a fold key would re-expand.
    #[serde(default)]
    pub header_selected: bool,
    pub rows: Vec<RyeOsRowVm>,
}

// The timeline entry shapes live in `super::timeline`; re-exported here so
// the established `ui::view_model::RyeOsTimelineEntryVm` path is stable.
pub use super::timeline::RyeOsTimelineEntryVm;


#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsRowVm {
    pub id: String,
    pub primary: String,
    pub secondary: Option<String>,
    pub meta: Option<String>,
    pub kind: Option<String>,
    pub intent: Option<RyeOsUiIntent>,
    pub tone: RyeOsTone,
    pub selected: bool,
    #[serde(default)]
    pub expandable: bool,
    #[serde(default)]
    pub expanded: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub detail: Vec<RyeOsRowDetailVm>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsNoticeVm {
    pub id: String,
    pub message: String,
    pub tone: RyeOsTone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsTone {
    Good,
    Warn,
    Danger,
    #[default]
    Neutral,
    Accent,
}

pub fn build_view_model(core: &RyeOsCore) -> RyeOsViewModel {
    let session = session_vm(core);
    let health = health_label(core);
    let workspace = workspace_vm(core);
    let chrome = RyeOsChromeVm {
        title: "RyeOS".to_string(),
        subtitle: subtitle(core),
        health_label: health.clone(),
        health_tone: tone_for_health(&health),
    };
    RyeOsViewModel {
        schema_version: "ryeos.ui.vm.v1".to_string(),
        generation: core.generation,
        now_ms: core.runtime.now_ms,
        presentation: presentation_vm(core, &session, &chrome, &workspace),
        session,
        chrome,
        workspace,
        overlays: overlays(core),
        notices: core.notices_vm(),
    }
}

fn presentation_vm(
    core: &RyeOsCore,
    session: &RyeOsSessionVm,
    chrome: &RyeOsChromeVm,
    workspace: &RyeOsWorkspaceVm,
) -> RyeOsPresentationVm {
    let version = ryeos_version(core);
    RyeOsPresentationVm {
        schema_version: "ryeos.ui.presentation.v1".to_string(),
        theme: RyeOsThemeVm {
            id: "gruvbox-optic".to_string(),
            tone: RyeOsTone::Accent,
        },
        chrome: RyeOsPresentationChromeVm {
            title: "Rye OS".to_string(),
            version_label: format!("RYE OS - {version}"),
            border: core.style.border.name().to_string(),
            top_bar: top_bar_vm(core),
            status_bar: status_bar_vm(session, chrome, workspace, core, &version),
        },
        metrics: presentation_metrics_vm(core, workspace),
        frame: RyeOsFrameVm {
            corners: RyeOsFrameCornersVm {
                visible: true,
                tone: RyeOsTone::Accent,
            },
        },
        motion: core.ui.motion.clone(),
    }
}

fn top_bar_vm(core: &RyeOsCore) -> RyeOsTopBarVm {
    RyeOsTopBarVm {
        visible: core.ui.top_status_visible,
        tabs: core
            .workspaces
            .iter()
            .enumerate()
            .filter(|(index, workspace)| {
                *index == core.active_workspace || !workspace.center_is_empty()
            })
            .map(|(index, workspace)| RyeOsWorkspaceTabVm {
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

fn focused_tile_title(core: &RyeOsCore) -> String {
    core.workspace
        .tiles
        .get(&core.workspace.focused_tile)
        .map(|tile| tile_title(core, &tile.view))
        .unwrap_or_else(|| "home".to_string())
}

/// Tile/view-overlay title for a bound view: prefer the view item's authored
/// `name:` (content), fall back to the ref tail when none is declared. The
/// authored label is content too — "views are content" extends to the title.
fn tile_title(core: &RyeOsCore, view: &crate::workspace::ViewSpec) -> String {
    core.views
        .get(&view.view_ref)
        .and_then(|binding| binding.name.as_deref())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| view.title())
}

fn layout_symbol(core: &RyeOsCore) -> String {
    let total = core.workspace.tile_ids().len();
    let master = core.workspace.tiling.master.count.min(total);
    let slave = total.saturating_sub(master);
    format!("M{master}│S{slave}")
}

fn presentation_metrics_vm(
    core: &RyeOsCore,
    workspace: &RyeOsWorkspaceVm,
) -> RyeOsPresentationMetricsVm {
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
        core.runtime.activity_pulse,
    );

    RyeOsPresentationMetricsVm {
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
    activity_pulse: f32,
) -> f32 {
    let active_threads = active_thread_count.max(0) as f32;
    ((tile_count as f32 * 0.12)
        + (motion_count as f32 * 0.22)
        + (loading_count as f32 * 0.18)
        + (active_threads * 0.18)
        + activity_pulse.clamp(0.0, 1.0) * 0.35)
        .clamp(0.0, 1.0)
}

/// Cumulative token usage for the loaded conversation, read from the
/// chain-replay source's `summary` block — the daemon's continuation-aware
/// chain totals. Summing the braid's `thread_usage` events instead would
/// over-count: each event carries the thread's cumulative-so-far totals
/// (reseeded across continuations), not a per-turn delta. `(0, 0)` when no
/// fetched source carries a usage summary.
fn conversation_usage(core: &RyeOsCore) -> (u64, u64) {
    let mut input = 0u64;
    let mut output = 0u64;
    for source in core.data.sources.values() {
        let Some(summary) = source.get("summary") else {
            continue;
        };
        input += summary
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        output += summary
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
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
    session: &RyeOsSessionVm,
    chrome: &RyeOsChromeVm,
    workspace: &RyeOsWorkspaceVm,
    core: &RyeOsCore,
    version: &str,
) -> RyeOsStatusBarVm {
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
    let key_hint = if core.workspace.lens_stack.is_empty() {
        "ctrl+k open · alt+s shards · alt+t/b bars · ctrl+←/→ tab · ctrl+↑/↓ move".to_string()
    } else {
        "⌫ back · alt+← back · ctrl+k open · alt+s shards · alt+t/b bars · ctrl+←/→ tab · ctrl+↑/↓ move"
            .to_string()
    };
    RyeOsStatusBarVm {
        visible: core.ui.bottom_status_visible,
        segments: vec![
            RyeOsStatusSegmentVm {
                id: "brand".to_string(),
                label: None,
                value: "rye os".to_string(),
                tone: RyeOsTone::Accent,
                grow: false,
            },
            RyeOsStatusSegmentVm {
                id: "version".to_string(),
                label: None,
                value: format!("v{version}"),
                tone: RyeOsTone::Neutral,
                grow: false,
            },
            RyeOsStatusSegmentVm {
                id: "health".to_string(),
                label: None,
                value: chrome.health_label.clone(),
                tone: chrome.health_tone,
                grow: false,
            },
            RyeOsStatusSegmentVm {
                id: "mode".to_string(),
                label: None,
                value: if session.read_only { "ro" } else { "rw" }.to_string(),
                tone: RyeOsTone::Neutral,
                grow: false,
            },
            RyeOsStatusSegmentVm {
                id: "tiles".to_string(),
                label: Some("tiles".to_string()),
                value: workspace.tile_count.to_string(),
                tone: RyeOsTone::Neutral,
                grow: false,
            },
            RyeOsStatusSegmentVm {
                id: "items".to_string(),
                label: Some("items".to_string()),
                value: item_count.to_string(),
                tone: RyeOsTone::Neutral,
                grow: false,
            },
            RyeOsStatusSegmentVm {
                id: "threads".to_string(),
                label: Some("threads".to_string()),
                value: thread_count.to_string(),
                tone: RyeOsTone::Neutral,
                grow: false,
            },
            // Always-visible budget: the conversation's cumulative tokens
            // (input/output) summed from the braid's thread_usage events.
            RyeOsStatusSegmentVm {
                id: "usage".to_string(),
                label: Some("tokens".to_string()),
                value: format!("{}/{}", compact_count(usage_in), compact_count(usage_out)),
                tone: RyeOsTone::Neutral,
                grow: false,
            },
            RyeOsStatusSegmentVm {
                id: "principal".to_string(),
                label: Some("principal".to_string()),
                value: session
                    .user_principal_id
                    .as_deref()
                    .map(short_principal)
                    .unwrap_or_else(|| "local".to_string()),
                tone: RyeOsTone::Neutral,
                grow: false,
            },
            RyeOsStatusSegmentVm {
                id: "surface".to_string(),
                label: Some("surface".to_string()),
                value: short_surface_ref(&session.surface_ref),
                tone: RyeOsTone::Neutral,
                grow: false,
            },
            RyeOsStatusSegmentVm {
                id: "project".to_string(),
                label: None,
                value: session
                    .project_path
                    .clone()
                    .unwrap_or_else(|| "home".to_string()),
                tone: RyeOsTone::Neutral,
                grow: true,
            },
        ],
        key_hint,
        energy: core.runtime.activity_pulse.clamp(0.0, 1.0),
        attention: (core.runtime.now_ms < core.runtime.attention_until_ms)
            .then_some(RyeOsTone::Warn),
    }
}

fn workspace_vm(core: &RyeOsCore) -> RyeOsWorkspaceVm {
    let center_is_empty = core.workspace.center_is_empty();
    let backdrop_visible = center_is_empty || surface_uses_backdrop_underlay(core);
    RyeOsWorkspaceVm {
        root: core
            .workspace
            .layout()
            .map(|layout| layout_node_vm(&layout, core)),
        focused_tile: tile_id_text(core.workspace.focused_tile),
        center_is_empty,
        // The backdrop scene resolves for empty centers, and for populated
        // centers that opt into a translucent ambient underlay.
        backdrop: backdrop_visible.then(|| backdrop_scene(core)).flatten(),
        tile_count: core.workspace.tile_ids().len(),
        docks: dock_plane_vm(core),
        lens_trail: core
            .workspace
            .lens_stack
            .iter()
            .map(|frame| {
                frame
                    .label
                    .clone()
                    .unwrap_or_else(|| tile_title(core, &frame.view))
            })
            .collect(),
        lens_label: core.workspace.lens_label.clone(),
    }
}

/// Resolve the backdrop scene from the surface's `backdrop` view ref.
/// Absent `backdrop` → no scene (the background fill stands).
fn backdrop_scene(core: &RyeOsCore) -> Option<RyeOsSceneModel> {
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
    let mut scene = super::scene_model::scene_from_body(&binding.body, core.scene_frame());
    scene.break_amount = core.ui.backdrop_break_amount.clamp(0.0, 1.0);
    // The backdrop breathes with the node: live threads lift the scene's
    // energy, so the empty center visibly quickens while cognition runs
    // and settles back to calm when the node idles.
    let active_threads = core
        .data
        .dimension
        .as_ref()
        .map(|dimension| dimension.threads.active_count)
        .unwrap_or_default();
    // Keep project-local background activity from making the crystal race in
    // busy workspaces. Active threads should lift the scene slightly; transient
    // activity pulses carry the stronger "something just happened" signal.
    let active_component = ((active_threads.max(0) as f32) * 0.06).clamp(0.0, 0.24);
    scene.energy = active_component.max(core.runtime.activity_pulse.clamp(0.0, 1.0));
    Some(scene)
}

fn surface_uses_backdrop_underlay(core: &RyeOsCore) -> bool {
    core.data
        .session
        .as_ref()
        .and_then(|session| session.effective_surface.as_ref())
        .and_then(|value| serde_json::from_value::<SurfaceSpec>(value.clone()).ok())
        .and_then(|surface| surface.ambient)
        .is_some_and(|ambient| {
            ambient.show_background.unwrap_or(true)
                && ambient
                    .opacity
                    .is_some_and(|opacity| opacity > 0.0 && opacity < 1.0)
        })
}

fn dock_plane_vm(core: &RyeOsCore) -> RyeOsDockPlaneVm {
    RyeOsDockPlaneVm {
        top: dock_tile_vm(core, RyeOsDockEdge::Top, core.ui.docks.top.as_ref()),
        bottom: dock_tile_vm(core, RyeOsDockEdge::Bottom, core.ui.docks.bottom.as_ref()),
        left: dock_tile_vm(core, RyeOsDockEdge::Left, core.ui.docks.left.as_ref()),
        right: dock_tile_vm(core, RyeOsDockEdge::Right, core.ui.docks.right.as_ref()),
    }
}

fn dock_tile_vm(
    core: &RyeOsCore,
    edge: RyeOsDockEdge,
    state: Option<&RyeOsDockSlotState>,
) -> Option<RyeOsDockTileVm> {
    let state = state?;
    if !state.visible {
        return None;
    }
    let RyeOsDockContent::View { view_ref } = &state.content;
    let source_key = super::model::dock_source_key(edge);
    let focused = matches!(
        core.focus_target(),
        super::model::RyeOsFocusTarget::Dock { edge: focused } if focused == edge
    );
    let (cursor, collapsed, expanded_rows, changed_rows) = dock_selected_state(core, &source_key);
    Some(RyeOsDockTileVm {
        edge,
        title: view_ref.rsplit('/').next().unwrap_or(view_ref).to_string(),
        size: state.size,
        focused,
        view: bound_view_vm_keyed(
            core,
            &source_key,
            cursor,
            collapsed,
            expanded_rows,
            changed_rows,
            view_ref,
            &core.ui.atlas,
        ),
        input: instance_input_vm(core, &source_key, view_ref),
    })
}

type RowStateRefs<'a> = (
    Option<usize>,
    Option<&'a std::collections::BTreeSet<usize>>,
    Option<&'a std::collections::BTreeSet<String>>,
    Option<&'a std::collections::BTreeMap<String, u64>>,
);

fn dock_selected_state<'a>(core: &'a RyeOsCore, source_key: &str) -> RowStateRefs<'a> {
    match core.ui.dock_local.get(source_key) {
        Some(crate::workspace::ViewLocalState::GenericList {
            cursor,
            collapsed,
            expanded_rows,
            changed_rows,
            ..
        }) => (
            Some(*cursor),
            Some(collapsed),
            Some(expanded_rows),
            Some(changed_rows),
        ),
        _ => (None, None, None, None),
    }
}

/// The input prompt VM for a view instance, if its binding declares an
/// `input` block. Re-sources the active transient buffer; layout-neutral.
fn instance_input_vm(core: &RyeOsCore, instance_id: &str, view_ref: &str) -> Option<RyeOsInputVm> {
    let binding = core.views.get(view_ref)?;
    let input = binding.input.as_ref()?;
    let key = super::model::InputBufferKey::new(instance_id, view_ref, input.id.clone());
    Some(input_vm(core, &key, view_ref, input))
}

/// Render a content-bound view: binding + source response -> widget VM.
/// Pure projection; unknown widgets and missing data degrade honestly.
fn bound_view_vm(core: &RyeOsCore, tile_id: TileId, view_ref: &str) -> RyeOsViewVm {
    let (expanded_rows, changed_rows) = selected_row_state(core, tile_id);
    bound_view_vm_keyed(
        core,
        &tile_id.0.to_string(),
        selected_cursor(core, tile_id),
        selected_collapsed(core, tile_id),
        expanded_rows,
        changed_rows,
        view_ref,
        core.tile_atlas_state(tile_id),
    )
}

fn bound_view_vm_keyed(
    core: &RyeOsCore,
    source_key: &str,
    cursor: Option<usize>,
    collapsed: Option<&std::collections::BTreeSet<usize>>,
    expanded_rows: Option<&std::collections::BTreeSet<String>>,
    changed_rows: Option<&std::collections::BTreeMap<String, u64>>,
    view_ref: &str,
    atlas: &crate::atlas::AtlasUiStateVm,
) -> RyeOsViewVm {
    let Some(binding) = core.views.get(view_ref) else {
        return RyeOsViewVm::Placeholder {
            title: view_ref.to_string(),
            message: format!("view {view_ref} is not embedded in the effective surface"),
        };
    };
    // A binding that failed to parse/validate shows its reason, not its
    // (absent) content — honest degrade, not a silent "not embedded".
    if let Some(reason) = &binding.degraded {
        return RyeOsViewVm::Placeholder {
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
            return RyeOsViewVm::Atlas {
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
            return RyeOsViewVm::Map {
                scene: build_scene_model(core, atlas, None, None),
            };
        }
        "sections" => {
            // A sections view reads one source per section (keyed by
            // section_source_key), not the single per-tile response below — so
            // it assembles here, ahead of the source-required arms.
            let title = view_ref.rsplit('/').next().unwrap_or(view_ref).to_string();
            if binding.sections.is_empty() {
                return RyeOsViewVm::Placeholder {
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
                        .find(|a| {
                            a.get("id").and_then(|v| v.as_str()) == Some(affordance_id.as_str())
                        })
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
                        rows.push(RyeOsRowVm {
                            id: format!("{view_ref}#{index}#{}", rows.len()),
                            primary: record.primary,
                            secondary: None,
                            meta: record.meta,
                            kind: None,
                            intent: activate.map(|affordance_id| RyeOsUiIntent::InvokeAffordance {
                                view_ref: view_ref.to_string(),
                                affordance_id: affordance_id.clone(),
                                record: record.raw.clone(),
                            }),
                            tone: tone_from_name(record.tone.as_deref()),
                            selected,
                            expandable: false,
                            expanded: false,
                            detail: Vec::new(),
                            changed_at_ms: None,
                        });
                    }
                }
                sections.push(RyeOsSectionVm {
                    title: section.title.clone(),
                    count,
                    collapsed: is_collapsed,
                    header_selected,
                    rows,
                });
            }
            return RyeOsViewVm::Sections {
                title,
                sections,
                fold_section,
            };
        }
        _ => {}
    }
    // A view may render a seat facet directly (no service fetch) — e.g. the
    // inspector showing `selection.summary`, an inline event detail written by
    // an inspect intent. The facet wins when it resolves; otherwise the view
    // falls back to its fetched `source` response.
    let facet_response = binding
        .facet
        .as_deref()
        .and_then(|facet| facet_backed_response(core, facet));
    let response = facet_response
        .as_ref()
        .or_else(|| core.data.sources.get(source_key));
    let title = view_ref.rsplit('/').next().unwrap_or(view_ref).to_string();
    if binding.widget == "text" {
        if let Some(lines) = static_text_lines(binding) {
            return RyeOsViewVm::Text {
                title,
                lines,
                position: text_position(binding),
            };
        }
    }
    match (binding.widget.as_str(), response) {
        // A feed with no chain root is empty, not loading — it would spin
        // forever on a fetch that never resolves (no chain root to replay).
        // The feed follows `chain_root` (the whole braid), so key the empty
        // state off that, not the moving head. Show an honest
        // start-a-conversation state instead.
        ("timeline", None) if core.seat.fold().input_route().chain_root.is_none() => {
            RyeOsViewVm::Placeholder {
                title,
                message: "No conversation yet — type below to start one.".to_string(),
            }
        }
        (_, None) => RyeOsViewVm::Placeholder {
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
            let expand_fields = super::content::expand_fields(binding);
            let records = super::content::source_collection(binding, response);
            let total_rows = records.len();
            let (start, end) = row_render_window(total_rows, cursor);
            let selected_index = clamped_row_cursor(total_rows, cursor);
            let rows = records[start..end]
                .iter()
                .enumerate()
                .map(|(offset, raw)| {
                    let index = start + offset;
                    let record = super::content::project_record_for_binding(binding, raw);
                    let key = super::model::row_key(&record.raw, index);
                    let expanded = expanded_rows.is_some_and(|set| set.contains(&key));
                    let detail = expanded
                        .then(|| detail_vm(&record.raw, &expand_fields))
                        .unwrap_or_default();
                    RyeOsRowVm {
                        id: format!("{view_ref}#{index}"),
                        primary: record.primary,
                        secondary: None,
                        meta: record.meta,
                        kind: None,
                        intent: activate_affordance.as_ref().map(|affordance_id| {
                            RyeOsUiIntent::InvokeAffordance {
                                view_ref: view_ref.to_string(),
                                affordance_id: affordance_id.clone(),
                                record: record.raw.clone(),
                            }
                        }),
                        tone: tone_from_name(record.tone.as_deref()),
                        selected: selected_index == Some(index),
                        expandable: !expand_fields.is_empty(),
                        expanded,
                        detail,
                        changed_at_ms: changed_rows.and_then(|rows| rows.get(&key).copied()),
                    }
                })
                .collect();
            RyeOsViewVm::Rows {
                title,
                columns: Vec::new(),
                total_rows,
                provenance: Some(view_ref.to_string()),
                affordance_hints: affordance_hints(binding),
                rows,
            }
        }
        ("timeline", Some(response)) => {
            use std::borrow::Cow;

            let cached = core.data.timeline_sources.get(source_key);
            let (full, full_indents, full_sources, full_sections, collapsible) =
                if let Some(cache) = cached {
                    (
                        Cow::Borrowed(cache.entries.as_slice()),
                        Cow::Borrowed(cache.indents.as_slice()),
                        Cow::Borrowed(cache.sources.as_slice()),
                        Cow::Borrowed(cache.sections.as_slice()),
                        Cow::Borrowed(&cache.collapsible),
                    )
                } else {
                    let (mut entries, mut indents, mut sources) = timeline_entries_indented(
                        super::content::project_records(binding, response),
                    );
                    if let Some(summary) = timeline_summary_entry(response) {
                        entries.insert(0, summary);
                        indents.insert(0, 0);
                        sources.insert(0, None);
                    }
                    let (sections, collapsible_set) =
                        super::timeline::timeline_section_index(&entries);
                    (
                        Cow::Owned(entries),
                        Cow::Owned(indents),
                        Cow::Owned(sources),
                        Cow::Owned(sections),
                        Cow::Owned(collapsible_set),
                    )
                };
            // Apply the operator's folds, then project over the VISIBLE list so
            // the cursor, scroll, and point all address what's actually shown.
            let empty = std::collections::BTreeSet::new();
            let windowed = super::timeline::fold_timeline_window(
                full.as_ref(),
                full_indents.as_ref(),
                full_sources.as_ref(),
                full_sections.as_ref(),
                collapsible.as_ref(),
                super::timeline::live_delta_entry(core),
                collapsed.unwrap_or(&empty),
                cursor.unwrap_or(0),
                TIMELINE_RENDER_WINDOW,
            );
            let folded = windowed.folded;
            let selected = windowed.selected;
            // The foldable section under the point — what a fold key toggles.
            let fold_section = selected.and_then(|i| {
                let section = folded.sections.get(i).copied()?;
                folded.collapsible.contains(&section).then_some(section)
            });
            let expand_fields = super::content::expand_fields(binding);
            let entry_expandable: Vec<bool> = folded
                .sources
                .iter()
                .map(|source| source.is_some() && !expand_fields.is_empty())
                .collect();
            let entry_expanded: Vec<bool> = folded
                .sources
                .iter()
                .map(|source| {
                    source.as_ref().is_some_and(|source| {
                        expanded_rows.is_some_and(|set| set.contains(&source.key))
                    })
                })
                .collect();
            let entry_details: Vec<Vec<RyeOsRowDetailVm>> =
                if entry_expanded.iter().any(|expanded| *expanded) {
                    folded
                        .sources
                        .iter()
                        .zip(entry_expanded.iter())
                        .map(|(source, expanded)| {
                            if !*expanded {
                                Vec::new()
                            } else {
                                source
                                    .as_ref()
                                    .map(|source| detail_vm(&source.raw, &expand_fields))
                                    .unwrap_or_default()
                            }
                        })
                        .collect()
                } else {
                    Vec::new()
                };
            RyeOsViewVm::Timeline {
                title,
                provenance: Some(view_ref.to_string()),
                affordance_hints: affordance_hints(binding),
                entries: folded.entries,
                entry_indents: folded.indents,
                selected,
                fold_section,
                entry_expandable,
                entry_expanded,
                entry_details,
            }
        }
        ("table", Some(response)) => {
            // The typed list surface: columns + per-column field projections,
            // declared by the binding (the engine knows only the `table`
            // widget). Row activation is the same explicit `selection.activate`
            // affordance the rows widget uses.
            let activate_affordance = activate_affordance(binding);
            let columns = super::content::table_columns(binding);
            let expand_fields = super::content::expand_fields(binding);
            let column_labels = columns.iter().map(|col| col.label.clone()).collect();
            let records = super::content::source_collection(binding, response);
            let total_rows = records.len();
            let (start, end) = row_render_window(total_rows, cursor);
            let selected_index = clamped_row_cursor(total_rows, cursor);
            let rows = records[start..end]
                .iter()
                .enumerate()
                .map(|(offset, raw)| {
                    let index = start + offset;
                    let record = super::content::project_table_record(binding, raw, &columns);
                    let key = super::model::row_key(&record.raw, index);
                    let expanded = expanded_rows.is_some_and(|set| set.contains(&key));
                    let detail = expanded
                        .then(|| detail_vm(&record.raw, &expand_fields))
                        .unwrap_or_default();
                    RyeOsTableRowVm {
                        id: format!("{view_ref}#{index}"),
                        cells: record.cells,
                        cell_tones: if record.cell_tones.iter().all(Option::is_none) {
                            Vec::new()
                        } else {
                            record
                                .cell_tones
                                .iter()
                                .map(|tone| tone.as_deref().map(|t| tone_from_name(Some(t))))
                                .collect()
                        },
                        tone: tone_from_name(record.tone.as_deref()),
                        intent: activate_affordance.as_ref().map(|affordance_id| {
                            RyeOsUiIntent::InvokeAffordance {
                                view_ref: view_ref.to_string(),
                                affordance_id: affordance_id.clone(),
                                record: record.raw.clone(),
                            }
                        }),
                        selected: selected_index == Some(index),
                        expandable: !expand_fields.is_empty(),
                        expanded,
                        detail,
                        changed_at_ms: changed_rows.and_then(|rows| rows.get(&key).copied()),
                        raw: record.raw,
                    }
                })
                .collect();
            RyeOsViewVm::Table {
                title,
                columns: column_labels,
                total_rows,
                provenance: Some(view_ref.to_string()),
                affordance_hints: affordance_hints(binding),
                rows,
            }
        }
        ("key_value" | "text", Some(response)) => {
            let mut rows: Vec<RyeOsRowVm> = super::content::project_detail(binding, response)
                .into_iter()
                .enumerate()
                .map(|(index, (key, value))| RyeOsRowVm {
                    id: format!("{view_ref}#{key}"),
                    primary: format!("{key}: {value}"),
                    secondary: None,
                    meta: None,
                    kind: None,
                    intent: None,
                    tone: RyeOsTone::Neutral,
                    selected: cursor == Some(index),
                    expandable: false,
                    expanded: false,
                    detail: Vec::new(),
                    changed_at_ms: None,
                })
                .collect();
            let notice_start = rows.len();
            rows.extend(status_notice_rows(core, view_ref, notice_start, cursor));
            RyeOsViewVm::Rows {
                title,
                columns: Vec::new(),
                total_rows: rows.len(),
                provenance: Some(view_ref.to_string()),
                affordance_hints: affordance_hints(binding),
                rows,
            }
        }
        (other, Some(_)) => RyeOsViewVm::Placeholder {
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

fn status_notice_rows(
    core: &RyeOsCore,
    view_ref: &str,
    start_index: usize,
    cursor: Option<usize>,
) -> Vec<RyeOsRowVm> {
    if !is_status_view_ref(view_ref) {
        return Vec::new();
    }
    core.ui
        .notices
        .iter()
        .rev()
        .take(4)
        .enumerate()
        .map(|(offset, notice)| RyeOsRowVm {
            id: format!("{view_ref}#{}", notice.id),
            primary: notice.message.clone(),
            secondary: None,
            meta: Some("notice".to_string()),
            kind: None,
            intent: None,
            tone: notice.tone,
            selected: cursor == Some(start_index + offset),
            expandable: false,
            expanded: false,
            detail: Vec::new(),
            changed_at_ms: None,
        })
        .collect()
}

fn is_status_view_ref(view_ref: &str) -> bool {
    matches!(
        view_ref.strip_prefix("view:").unwrap_or(view_ref),
        "ryeos/node/status" | "ryeos/ui/status"
    )
}

fn row_render_window(total: usize, cursor: Option<usize>) -> (usize, usize) {
    const ROW_WINDOW: usize = 96;
    if total <= ROW_WINDOW {
        return (0, total);
    }
    let cursor = clamped_row_cursor(total, cursor).unwrap_or(0);
    let start = cursor
        .saturating_sub(ROW_WINDOW / 2)
        .min(total.saturating_sub(ROW_WINDOW));
    (start, start + ROW_WINDOW)
}

const TIMELINE_RENDER_WINDOW: usize = 192;

fn clamped_row_cursor(total: usize, cursor: Option<usize>) -> Option<usize> {
    (total > 0).then(|| cursor.unwrap_or(0).min(total - 1))
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
#[cfg(test)]
pub(crate) use super::timeline::append_live_delta;
#[cfg(test)]
pub(crate) use super::timeline::timeline_entries;
pub(crate) use super::timeline::timeline_entries_indented;

pub(crate) fn tone_from_name(name: Option<&str>) -> RyeOsTone {
    match name {
        Some("accent") => RyeOsTone::Accent,
        Some("good") => RyeOsTone::Good,
        Some("warn") => RyeOsTone::Warn,
        Some("danger") => RyeOsTone::Danger,
        _ => RyeOsTone::Neutral,
    }
}

fn detail_vm(record: &serde_json::Value, fields: &[String]) -> Vec<RyeOsRowDetailVm> {
    super::content::expanded_detail(record, fields)
        .into_iter()
        .map(|(field, value)| RyeOsRowDetailVm {
            field,
            value,
            tone: None,
        })
        .collect()
}

/// Project a view instance's input buffer into the prompt VM. The target
/// strip is `target_label` if authored, else derived from the bound submit
/// target (the seat route for `submit: route`; the affordance's invoke
/// target for `submit: <affordance>`).
fn input_vm(
    core: &RyeOsCore,
    key: &super::model::InputBufferKey,
    view_ref: &str,
    input: &super::content::InputBlock,
) -> RyeOsInputVm {
    let buffer = core.ui.input_buffers.get(&key.storage_key());
    let text = buffer.map(|b| b.text.clone()).unwrap_or_default();
    let cursor = buffer.map(|b| b.cursor).unwrap_or(0);
    let focused = core
        .focused_input_instance()
        .as_ref()
        .is_some_and(|(focused_key, _)| focused_key.storage_key() == key.storage_key());

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
    RyeOsInputVm {
        focused,
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
pub(crate) fn timeline_summary_entry(response: &serde_json::Value) -> Option<RyeOsTimelineEntryVm> {
    let summary = response.get("summary")?;
    let status = summary.get("status").and_then(|v| v.as_str()).unwrap_or("");
    let input = summary
        .get("input_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let output = summary
        .get("output_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let cost = summary
        .get("spend_usd")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let turns = summary.get("turns").and_then(|v| v.as_i64()).unwrap_or(0);
    let primary = format!("{status} · ↑{input} ↓{output} · ${cost:.4} · {turns} turns");
    Some(RyeOsTimelineEntryVm::Line {
        primary,
        meta: None,
        tone: status_tone(status),
        intent: None,
        secondary_intent: None,
    })
}

/// The response a facet-backed view renders: the seat-fold value at `facet`,
/// resolved through the shared `@facet:` grammar (so a dotted path like
/// `selection.summary` reads the field within the `selection` facet). `None`
/// when the facet is unset — the view then falls back to its `source` fetch.
fn facet_backed_response(core: &RyeOsCore, facet: &str) -> Option<serde_json::Value> {
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
fn status_tone(status: &str) -> RyeOsTone {
    use super::dto::ThreadStatus;
    match ThreadStatus::from_wire(status) {
        ThreadStatus::Running | ThreadStatus::Created => RyeOsTone::Accent,
        ThreadStatus::Failed | ThreadStatus::Killed | ThreadStatus::TimedOut => RyeOsTone::Danger,
        ThreadStatus::Cancelled => RyeOsTone::Warn,
        ThreadStatus::Completed | ThreadStatus::Continued => RyeOsTone::Good,
        ThreadStatus::Unknown => RyeOsTone::Neutral,
    }
}

/// Derive the target strip when the author gives no `target_label`.
fn derived_target_label(
    core: &RyeOsCore,
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
    core: &RyeOsCore,
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

fn layout_node_vm(node: &LayoutTree, core: &RyeOsCore) -> RyeOsLayoutNodeVm {
    match node {
        LayoutTree::Leaf(tile_id) => {
            let view = core
                .workspace
                .tiles
                .get(tile_id)
                .map(|tile| view_vm(core, *tile_id, tile))
                .unwrap_or_else(|| RyeOsViewVm::Placeholder {
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
            let chrome_hidden = core
                .workspace
                .tiles
                .get(tile_id)
                .is_some_and(|tile| view_hides_tile_chrome(core, &tile.view.view_ref));
            let background_transparent = core
                .workspace
                .tiles
                .get(tile_id)
                .is_some_and(|tile| view_has_transparent_background(core, &tile.view.view_ref));
            RyeOsLayoutNodeVm::Tile {
                tile_id: tile_id_text(*tile_id),
                focused: *tile_id == core.workspace.focused_tile,
                title,
                intents: tile_intents(core, *tile_id),
                view,
                chrome_hidden,
                background_transparent,
                input,
            }
        }
        LayoutTree::Split {
            axis,
            ratio,
            first,
            second,
        } => RyeOsLayoutNodeVm::Split {
            axis: match axis {
                SplitAxis::Horizontal => RyeOsSplitAxisVm::Horizontal,
                SplitAxis::Vertical => RyeOsSplitAxisVm::Vertical,
            },
            ratio: *ratio,
            first: Box::new(layout_node_vm(first, core)),
            second: Box::new(layout_node_vm(second, core)),
        },
    }
}

fn static_text_lines(binding: &super::content::ViewBinding) -> Option<Vec<RyeOsTextLineVm>> {
    let lines = binding.body.get("lines")?.as_array()?;
    let out: Vec<RyeOsTextLineVm> = lines
        .iter()
        .filter_map(|line| {
            if let Some(text) = line.as_str() {
                return Some(RyeOsTextLineVm {
                    text: text.to_string(),
                    tone: RyeOsTone::Neutral,
                });
            }
            let text = line.get("text").and_then(serde_json::Value::as_str)?;
            Some(RyeOsTextLineVm {
                text: text.to_string(),
                tone: tone_from_name(line.get("tone").and_then(serde_json::Value::as_str)),
            })
        })
        .collect();
    (!out.is_empty()).then_some(out)
}

fn view_hides_tile_chrome(core: &RyeOsCore, view_ref: &str) -> bool {
    core.views
        .get(view_ref)
        .and_then(|binding| binding.presentation.chrome)
        .is_some_and(|chrome| matches!(chrome, super::content::ViewChromePresentation::None))
}

fn view_has_transparent_background(core: &RyeOsCore, view_ref: &str) -> bool {
    core.views
        .get(view_ref)
        .and_then(|binding| binding.presentation.background)
        .is_some_and(|background| {
            matches!(
                background,
                super::content::ViewBackgroundPresentation::Transparent
            )
        })
}

fn text_position(binding: &super::content::ViewBinding) -> RyeOsTextPositionVm {
    let pos = binding.presentation.position;
    RyeOsTextPositionVm {
        x: pos.map(|p| p.x).unwrap_or(0.5).clamp(0.0, 1.0),
        y: pos.map(|p| p.y).unwrap_or(0.5).clamp(0.0, 1.0),
    }
}

fn view_vm(core: &RyeOsCore, tile_id: TileId, tile: &TileState) -> RyeOsViewVm {
    // Every tile is a bound view; the scene widgets (graph/atlas) dispatch
    // by `widget` inside `bound_view_vm`.
    bound_view_vm(core, tile_id, &tile.view.view_ref)
}

fn session_vm(core: &RyeOsCore) -> RyeOsSessionVm {
    let browser = core.data.session.as_ref();
    let dimension = core.data.dimension.as_ref();
    RyeOsSessionVm {
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

fn ambient_vm(core: &RyeOsCore) -> RyeOsAmbientVm {
    let Some(surface) = core
        .data
        .session
        .as_ref()
        .and_then(|session| session.effective_surface.as_ref())
        .and_then(|value| serde_json::from_value::<SurfaceSpec>(value.clone()).ok())
    else {
        return RyeOsAmbientVm {
            show_background: true,
            opacity: None,
            mode: RyeOsAmbientModeVm::Ambient,
            atlas: None,
        };
    };
    let Some(ambient) = surface.ambient else {
        return RyeOsAmbientVm {
            show_background: true,
            opacity: None,
            mode: RyeOsAmbientModeVm::Ambient,
            atlas: None,
        };
    };
    let atlas_style = ambient.namespace_atlas_style();
    RyeOsAmbientVm {
        show_background: ambient.show_background.unwrap_or(true),
        opacity: ambient.opacity,
        mode: if atlas_style.is_some() {
            RyeOsAmbientModeVm::NamespaceAtlas
        } else {
            RyeOsAmbientModeVm::Ambient
        },
        atlas: atlas_style.map(|style| RyeOsAmbientAtlasVm {
            style: match style {
                AmbientAtlasStyleSpec::Flat2d => RyeOsAmbientAtlasStyleVm::Flat2d,
                AmbientAtlasStyleSpec::Paper3d => RyeOsAmbientAtlasStyleVm::Paper3d,
            },
        }),
    }
}

/// Launchable views: every view embedded in the effective surface,
/// graph/atlas included (they are ordinary `view:` items now). Nothing
/// here names a product concept — labels and hints come from the items.
pub(crate) fn view_overlay_items(core: &RyeOsCore) -> Vec<RyeOsOverlayItemVm> {
    let mut items: Vec<RyeOsOverlayItemVm> = Vec::new();
    for group in core.library_groups() {
        if group.refs.is_empty() {
            continue;
        }
        let expanded = !core.ui.overlay.collapsed.contains(&group.title);
        items.push(RyeOsOverlayItemVm {
            category: group.title.clone(),
            primary: group.title.clone(),
            enabled: true,
            intent: Some(RyeOsUiIntent::ToggleOverlayGroup {
                group: group.title.clone(),
            }),
            header: true,
            expanded,
            ..Default::default()
        });
        for view_ref in group.refs {
            let Some(binding) = core.views.get(&view_ref) else {
                continue;
            };
            let missing = unsatisfied_facets(core, binding);
            let enabled = missing.is_empty();
            let view = ViewSpec {
                view_ref: view_ref.clone(),
            };
            items.push(RyeOsOverlayItemVm {
                category: group.title.clone(),
                primary: launchable_view_label(&view_ref, binding),
                secondary: binding
                    .description
                    .clone()
                    .unwrap_or_else(|| binding.widget.clone()),
                meta: if enabled {
                    String::new()
                } else {
                    format!("needs {}", missing.join(", "))
                },
                enabled,
                intent: enabled.then(|| RyeOsUiIntent::OpenView { view: view.clone() }),
                secondary_intent: enabled.then(|| RyeOsUiIntent::OpenNewView { view }),
                depth: 1,
                ..Default::default()
            });
        }
    }
    items
}

/// A view's launcher label, from content: authored `title:`, else the
/// authored `name:`, else the stripped ref.
fn launchable_view_label(view_ref: &str, binding: &ViewBinding) -> String {
    binding
        .title
        .as_deref()
        .or(binding.name.as_deref())
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| view_ref.strip_prefix("view:").unwrap_or(view_ref))
        .to_string()
}

/// The `@facet:` references in a view's declarations that the current
/// seat fold cannot resolve. Defaulting refs (`@facet:x|…`) never gate —
/// they resolve to their default. Generic over the grammar; the engine
/// never names a facet.
pub(crate) fn unsatisfied_facets(core: &RyeOsCore, binding: &ViewBinding) -> Vec<String> {
    let mut refs: Vec<String> = Vec::new();
    collect_required_facet_refs(&binding.body, &mut refs);
    if let Some(source) = &binding.source {
        collect_required_facet_refs(&source.params, &mut refs);
    }
    refs.sort();
    refs.dedup();
    let fold = core.seat.fold();
    refs.retain(|spec| {
        super::content::resolve_params(
            &serde_json::Value::String(format!("@facet:{spec}")),
            |key| fold.get(key).cloned(),
        )
        .is_null()
    });
    refs
}

fn collect_required_facet_refs(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => {
            if let Some(rest) = s.strip_prefix("@facet:") {
                if !rest.contains('|') {
                    out.push(rest.to_string());
                }
            }
        }
        serde_json::Value::Object(map) => {
            for nested in map.values() {
                collect_required_facet_refs(nested, out);
            }
        }
        serde_json::Value::Array(items) => {
            for nested in items {
                collect_required_facet_refs(nested, out);
            }
        }
        _ => {}
    }
}

fn dock_command_items(core: &RyeOsCore) -> Vec<RyeOsOverlayChoice> {
    // Only surface-declared slots are toggleable; absent edges have no
    // slot and offer nothing. Labels stay mechanism words (edge names).
    [
        (
            RyeOsDockEdge::Bottom,
            "bottom",
            core.ui.docks.bottom.as_ref(),
        ),
        (RyeOsDockEdge::Left, "left", core.ui.docks.left.as_ref()),
        (RyeOsDockEdge::Right, "right", core.ui.docks.right.as_ref()),
        (RyeOsDockEdge::Top, "top", core.ui.docks.top.as_ref()),
    ]
    .into_iter()
    .filter_map(|(edge, name, slot)| slot.map(|slot| (edge, name, slot.visible)))
    .map(|(edge, name, visible)| RyeOsOverlayChoice {
        label: format!("{} {name} slot", if visible { "Hide" } else { "Show" }),
        hint: "toggle edge slot".to_string(),
        intent: RyeOsUiIntent::ToggleDock { edge },
        secondary_intent: None,
        enabled: true,
    })
    .collect()
}

/// Focused table row affordances OTHER than its activate (Enter) intent.
/// Each is rebuilt as an `InvokeAffordance` from the row's raw record and
/// the view's declared affordances.
fn focused_row_command_items(core: &RyeOsCore) -> Vec<RyeOsOverlayChoice> {
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
                return None; // already the row's Enter intent
            }
            let label = aff
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or(id)
                .to_string();
            Some(RyeOsOverlayChoice {
                label,
                hint: "focused row".to_string(),
                intent: RyeOsUiIntent::InvokeAffordance {
                    view_ref: view_ref.clone(),
                    affordance_id: id.to_string(),
                    record: row.raw.clone(),
                },
                secondary_intent: None,
                enabled: true,
            })
        })
        .collect()
}

fn context_command_items(core: &RyeOsCore) -> Vec<RyeOsOverlayChoice> {
    let mut items = Vec::new();
    items.push(RyeOsOverlayChoice {
        label: "Toggle backdrop break".to_string(),
        hint: "toggle the current backdrop scene between together and apart".to_string(),
        intent: RyeOsUiIntent::ToggleBackdropBreak,
        secondary_intent: None,
        enabled: true,
    });

    // A recoverable failed terminal offers retry: re-submit its own stimulus as
    // a continuation, retargeted at the selected failed thread, pre-filled for
    // review (not one-click). Surfaced two ways so it works everywhere — as the
    // Inspect item's Shift+Enter secondary, AND as a distinct plain-Enter item
    // (clients that can't send Shift+Enter still reach it).
    let retry = retry_intent_for_focused_row(core);

    if let Some(intent) = inspect_intent_for_focused_row(core) {
        let hint = if retry.is_some() {
            "Enter inspect · Shift+Enter retry".to_string()
        } else {
            focused_selection_hint(core).unwrap_or_else(|| "focused row".to_string())
        };
        items.push(RyeOsOverlayChoice {
            label: "Inspect selection".to_string(),
            hint,
            intent,
            secondary_intent: retry.clone(),
            enabled: true,
        });
    }

    if let Some(intent) = retry {
        items.push(RyeOsOverlayChoice {
            label: "Retry failed turn".to_string(),
            hint: "re-submit this failed turn (review, then Enter)".to_string(),
            intent,
            secondary_intent: None,
            enabled: true,
        });
    }

    // The focused row's non-activate affordances (e.g. Cancel on a thread row),
    // so row management is reachable — the row's Enter intent is only the
    // activate affordance.
    items.extend(focused_row_command_items(core));

    // Steering the active execution: offered only when the route has a head
    // thread. Each dispatches the shared SubmitThreadCommand → commands/submit.
    if let Some(head) = core.seat.fold().input_route().thread {
        // "continue" is an operator follow-up — gate it on the substrate fact so
        // a machine-only thread (graph) doesn't offer an operator continue the
        // daemon refuses. "cancel" (terminate) applies to any active thread.
        //
        // No command-style "interrupt" item: the operator interrupts a running
        // directive by submitting text with Alt+Enter (a live cognition_in
        // redirect via threads/input) — "Interrupt" is reserved for that.
        use crate::ui::dto::ThreadControlCommand;
        let operator_continuable = core.thread_supports_operator_followup(&head) != Some(false);
        for (label, command) in [
            ("Continue thread", ThreadControlCommand::Continue),
            ("Cancel thread", ThreadControlCommand::Cancel),
        ] {
            items.push(RyeOsOverlayChoice {
                label: label.to_string(),
                hint: "active thread".to_string(),
                intent: RyeOsUiIntent::SubmitThreadCommand { command },
                secondary_intent: None,
                enabled: command != ThreadControlCommand::Continue || operator_continuable,
            });
        }
    }

    items
}

pub(crate) fn command_overlay_items_for(core: &RyeOsCore) -> Vec<RyeOsOverlayChoice> {
    let mut items = context_command_items(core);
    items.extend(dock_command_items(core));
    items
}

fn help_overlay_items() -> Vec<RyeOsOverlayItemVm> {
    let topic = |category: &str, primary: &str, secondary: &str| RyeOsOverlayItemVm {
        category: category.to_string(),
        primary: primary.to_string(),
        secondary: secondary.to_string(),
        meta: String::new(),
        enabled: false,
        intent: None,
        secondary_intent: None,
        ..Default::default()
    };
    let overlay =
        |category: &str, primary: &str, secondary: &str, overlay_id: &str| RyeOsOverlayItemVm {
            category: category.to_string(),
            primary: primary.to_string(),
            secondary: secondary.to_string(),
            meta: String::new(),
            enabled: true,
            intent: Some(RyeOsUiIntent::OpenOverlay {
                overlay_id: overlay_id.to_string(),
            }),
            secondary_intent: None,
            ..Default::default()
        };
    let view =
        |category: &str, primary: &str, secondary: &str, view_ref: &str| RyeOsOverlayItemVm {
            category: category.to_string(),
            primary: primary.to_string(),
            secondary: secondary.to_string(),
            meta: String::new(),
            enabled: true,
            intent: Some(RyeOsUiIntent::OpenView {
                view: ViewSpec {
                    view_ref: view_ref.to_string(),
                },
            }),
            secondary_intent: None,
            ..Default::default()
        };
    vec![
        overlay("Start", "Views", "Open the view launcher", "views"),
        overlay("Start", "Commands", "Open context commands", "commands"),
        overlay("Start", "Shortcuts", "Open the shortcut table", "shortcuts"),
        topic(
            "Start",
            "Input",
            "The foot input stays open while views move",
        ),
        view(
            "Work",
            "Projects",
            "Registered project contexts",
            "view:ryeos/projects/list",
        ),
        view(
            "Work",
            "Project threads",
            "Active-project threads with active/status/kind/source filters",
            "view:ryeos/threads/history",
        ),
        view(
            "Work",
            "Node threads",
            "Node-wide threads with active/status/kind/source filters",
            "view:ryeos/node/threads/history",
        ),
    ]
}

fn inspect_intent_for_focused_row(core: &RyeOsCore) -> Option<RyeOsUiIntent> {
    match intent_for_focused_row(core)? {
        intent @ (RyeOsUiIntent::InspectItem { .. }
        | RyeOsUiIntent::InspectThread { .. }
        | RyeOsUiIntent::InspectSummary { .. }
        | RyeOsUiIntent::ReadFile { .. }) => Some(intent),
        _ => None,
    }
}

fn focused_selection_hint(core: &RyeOsCore) -> Option<String> {
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

fn overlays(core: &RyeOsCore) -> Vec<RyeOsOverlayVm> {
    let Some(active) = core.ui.overlay.active.as_deref() else {
        return Vec::new();
    };
    let Some((title, widget, hint, _source_ref, columns)) = overlay_definition(core, active) else {
        return Vec::new();
    };
    let items = active_overlay_items(core);
    let selected = core.ui.overlay.selected.min(items.len().saturating_sub(1));
    vec![RyeOsOverlayVm {
        id: active.to_string(),
        title,
        widget,
        columns,
        query: core.ui.overlay.query.clone(),
        selected,
        hint,
        items,
    }]
}

fn overlay_definition(
    core: &RyeOsCore,
    id: &str,
) -> Option<(String, String, String, String, Vec<String>)> {
    let declared = core
        .data
        .session
        .as_ref()
        .and_then(|session| session.effective_surface.as_ref())
        .and_then(|value| serde_json::from_value::<SurfaceSpec>(value.clone()).ok())
        .and_then(|surface| surface.overlays.get(id).cloned());
    if let Some(spec) = declared {
        let source_ref = spec.source.map(|source| source.item_ref)?;
        return Some((
            if spec.title.is_empty() {
                id.to_string()
            } else {
                spec.title
            },
            if spec.widget.is_empty() {
                "palette".to_string()
            } else {
                spec.widget
            },
            spec.hint,
            source_ref,
            spec.columns,
        ));
    }
    Some(match id {
        "views" => (
            "Views".to_string(),
            "palette".to_string(),
            "type to filter views · enter to open · shift+enter for new tile".to_string(),
            "runtime:views/launchable".to_string(),
            Vec::new(),
        ),
        "commands" => (
            "Commands".to_string(),
            "palette".to_string(),
            "type to filter commands · enter to run · esc to close".to_string(),
            "runtime:commands/available".to_string(),
            Vec::new(),
        ),
        "help" => (
            "Help".to_string(),
            "table".to_string(),
            "type to filter help · esc to close".to_string(),
            "runtime:help".to_string(),
            vec!["Topic".to_string(), "Open".to_string()],
        ),
        "shortcuts" => (
            "Shortcuts".to_string(),
            "table".to_string(),
            "type to filter shortcuts · esc to close".to_string(),
            "runtime:shortcuts".to_string(),
            vec!["Keys".to_string(), "Intent".to_string()],
        ),
        _ => return None,
    })
}

pub(crate) fn active_overlay_items(core: &RyeOsCore) -> Vec<RyeOsOverlayItemVm> {
    let Some(active) = core.ui.overlay.active.as_deref() else {
        return Vec::new();
    };
    let Some((_, _, _, source_ref, _)) = overlay_definition(core, active) else {
        return Vec::new();
    };
    let query = core.ui.overlay.query.trim().to_lowercase();
    let items = overlay_source_items(core, &source_ref);
    if query.is_empty() {
        // Tree presentation: a collapsed header hides its children. Flat
        // sources carry no headers, so everything passes.
        let mut hidden = false;
        return items
            .into_iter()
            .filter(|item| {
                if item.header {
                    hidden = !item.expanded;
                    true
                } else {
                    !hidden
                }
            })
            .collect();
    }
    // A live search matches over every child regardless of fold state and
    // presents hits under their forced-open headers — a collapsed group
    // can never hide a match, and Enter always lands on one (headers go
    // inert while the query is live).
    let mut out: Vec<RyeOsOverlayItemVm> = Vec::new();
    let mut pending_header: Option<RyeOsOverlayItemVm> = None;
    for item in items {
        if item.header {
            pending_header = Some(item);
            continue;
        }
        let haystack = format!(
            "{} {} {} {}",
            item.category, item.primary, item.secondary, item.meta
        )
        .to_lowercase();
        if !haystack.contains(&query) {
            continue;
        }
        if let Some(mut header) = pending_header.take() {
            header.expanded = true;
            header.enabled = false;
            header.intent = None;
            header.secondary_intent = None;
            out.push(header);
        }
        out.push(item);
    }
    out
}

fn overlay_source_items(core: &RyeOsCore, source_ref: &str) -> Vec<RyeOsOverlayItemVm> {
    match source_ref {
        "runtime:commands/available" => command_overlay_items_for(core)
            .into_iter()
            .map(overlay_item_from_choice)
            .collect(),
        "runtime:help" => help_overlay_items(),
        "runtime:shortcuts" => shortcut_overlay_items(),
        "runtime:views/launchable" => view_overlay_items(core),
        _ => Vec::new(),
    }
}

fn overlay_item_from_choice(item: RyeOsOverlayChoice) -> RyeOsOverlayItemVm {
    let (category, primary) = choice_category_and_label(&item.label);
    RyeOsOverlayItemVm {
        category,
        primary,
        secondary: item.hint,
        meta: String::new(),
        enabled: item.enabled,
        intent: Some(item.intent),
        secondary_intent: item.secondary_intent,
        ..Default::default()
    }
}

fn shortcut_overlay_items() -> Vec<RyeOsOverlayItemVm> {
    shortcut_entries()
        .into_iter()
        .map(|entry| RyeOsOverlayItemVm {
            category: entry.category,
            primary: entry.keys,
            secondary: entry.description,
            meta: String::new(),
            enabled: false,
            intent: None,
            secondary_intent: None,
            ..Default::default()
        })
        .collect()
}

fn choice_category_and_label(label: &str) -> (String, String) {
    let trimmed = label.trim();
    if let Some((category, command)) = trimmed.rsplit_once('/') {
        return (category.to_string(), command.to_string());
    }
    if let Some((first, rest)) = trimmed.split_once(' ') {
        return (first.to_string(), rest.to_string());
    }
    ("View".to_string(), trimmed.to_string())
}

fn shortcut_entries() -> Vec<RyeOsShortcutEntryVm> {
    let entry = |category: &str, keys: &str, description: &str| RyeOsShortcutEntryVm {
        category: category.to_string(),
        keys: keys.to_string(),
        description: description.to_string(),
    };
    vec![
        entry(
            "Move",
            "↑ / ↓",
            "Move the point through rows; else move focus",
        ),
        entry(
            "Move",
            "← / →",
            "Expand row/feed details, fold sections, or move focus",
        ),
        entry(
            "Act",
            "Enter",
            "Activate the selected row (or steer-submit when typing)",
        ),
        entry(
            "Act",
            "Alt+Enter",
            "Submit as an interrupt — cut the running thread's turn and redirect",
        ),
        entry(
            "Act",
            "Tab / ⇧Tab",
            "Accept completion, else cycle the route target",
        ),
        entry("Act", "Esc", "Cancel a running thread; else close the lens"),
        entry("Lenses", "⌫ / Alt+←", "Return from a drill-in lens"),
        entry(
            "Input",
            "type",
            "The foot input is always live — text routes at the directive",
        ),
        entry(
            "Lenses",
            "Ctrl+K",
            "Open the view overlay (swap the center lens)",
        ),
        entry("Commands", "Ctrl+P", "Open the command overlay"),
        entry("Help", "Ctrl+H", "Open the help overlay"),
        entry("Shortcuts", "Ctrl+/", "Open the shortcuts overlay"),
        entry("Backdrop", "Ctrl+S", "Toggle backdrop break"),
        entry("Lenses", "Ctrl+← / →", "Switch workspace tab"),
        entry(
            "Move",
            "Ctrl+U / Ctrl+D",
            "Jump the focused rows by half a screen",
        ),
        entry("Layout", "Ctrl+↑ / ↓", "Move the focused tile in the stack"),
        entry("Layout", "Ctrl+⇧+arrows", "Resize the focused tile"),
        entry("Layout", "Alt+M", "Toggle the focused tile master / full"),
        entry(
            "Layout",
            "Alt+T / Alt+B",
            "Toggle the top / bottom status bar",
        ),
        entry("App", "Alt+Q", "Close the focused lens"),
        entry("App", "Ctrl+C", "Quit"),
    ]
}

fn tile_intents(core: &RyeOsCore, tile_id: TileId) -> Vec<RyeOsTileIntentVm> {
    // Dynamic tiling: the algorithm owns the tree; tiles offer no
    // manual splits. Closing the last tile returns home.
    let _ = core;
    let tile_id = tile_id_text(tile_id);
    vec![RyeOsTileIntentVm {
        label: "×".to_string(),
        title: "Close tile".to_string(),
        intent: RyeOsUiIntent::CloseTile { tile_id },
    }]
}

pub(crate) fn intent_for_focused_row(core: &RyeOsCore) -> Option<RyeOsUiIntent> {
    // Feed lens: activation acts on the entry under the point (e.g. enter a
    // forked subthread, inspect an error terminal), not a row.
    if let Some(entry) = focused_timeline_entry(core) {
        return match entry {
            RyeOsTimelineEntryVm::Line { intent, .. } => intent,
            _ => None,
        };
    }
    if let Some(intent) = focused_selected_row(core).and_then(|row| row.intent) {
        return Some(intent);
    }
    // Table lens: rows carry the same activation affordance, on a distinct VM.
    focused_selected_table_row(core).and_then(|row| row.intent)
}

/// The timeline entry under the point in the focused feed lens, if the focused
/// view is a timeline with a point on an entry. The single home for reading the
/// focused feed entry — both the Enter intent and command-overlay secondary
/// intents derive from it.
fn focused_timeline_entry(core: &RyeOsCore) -> Option<RyeOsTimelineEntryVm> {
    let tile_id = core.workspace.focused_tile;
    let view = core.workspace.focused_view()?;
    if let RyeOsViewVm::Timeline {
        entries, selected, ..
    } = bound_view_vm(core, tile_id, &view.view_ref)
    {
        return selected.and_then(|i| entries.into_iter().nth(i));
    }
    None
}

/// The focused feed entry's secondary affordance — the retry a recoverable
/// failed terminal carries. Surfaced through the commands overlay (its Shift+Enter
/// secondary and a distinct "Retry failed turn" item), never a direct feed key,
/// so Enter stays inspect.
fn retry_intent_for_focused_row(core: &RyeOsCore) -> Option<RyeOsUiIntent> {
    match focused_timeline_entry(core)? {
        RyeOsTimelineEntryVm::Line {
            secondary_intent, ..
        } => secondary_intent,
        _ => None,
    }
}

/// The row under the point in the focused tile, if the point is on a row. The
/// rows widget indexes by the flat cursor; sections carry the selection on the
/// row VM itself (the point may instead be on a collapsed header → no row).
/// Scene widgets (graph/atlas) have no rows.
fn focused_selected_row(core: &RyeOsCore) -> Option<RyeOsRowVm> {
    let tile_id = core.workspace.focused_tile;
    let view = core.workspace.focused_view()?;
    match bound_view_vm(core, tile_id, &view.view_ref) {
        RyeOsViewVm::Rows { rows, .. } => rows.into_iter().find(|row| row.selected),
        RyeOsViewVm::Sections { sections, .. } => sections
            .into_iter()
            .flat_map(|section| section.rows)
            .find(|row| row.selected),
        _ => None,
    }
}

/// The table row under the point in the focused tile. Table rows are a distinct
/// VM (`RyeOsTableRowVm`, columnar cells) from the rows widget, so they need
/// their own selection projection — same flat cursor, different shape.
fn focused_selected_table_row(core: &RyeOsCore) -> Option<RyeOsTableRowVm> {
    let tile_id = core.workspace.focused_tile;
    let view = core.workspace.focused_view()?;
    match bound_view_vm(core, tile_id, &view.view_ref) {
        RyeOsViewVm::Table { rows, .. } => rows.into_iter().find(|row| row.selected),
        _ => None,
    }
}

fn selected_cursor(core: &RyeOsCore, tile_id: TileId) -> Option<usize> {
    let tile = core.workspace.tiles.get(&tile_id)?;
    match &tile.local {
        ViewLocalState::GenericList { cursor, .. } => Some(*cursor),
        ViewLocalState::None => None,
    }
}

fn selected_collapsed(
    core: &RyeOsCore,
    tile_id: TileId,
) -> Option<&std::collections::BTreeSet<usize>> {
    match &core.workspace.tiles.get(&tile_id)?.local {
        ViewLocalState::GenericList { collapsed, .. } => Some(collapsed),
        ViewLocalState::None => None,
    }
}

fn selected_row_state(
    core: &RyeOsCore,
    tile_id: TileId,
) -> (
    Option<&std::collections::BTreeSet<String>>,
    Option<&std::collections::BTreeMap<String, u64>>,
) {
    match core.workspace.tiles.get(&tile_id).map(|tile| &tile.local) {
        Some(ViewLocalState::GenericList {
            expanded_rows,
            changed_rows,
            ..
        }) => (Some(expanded_rows), Some(changed_rows)),
        _ => (None, None),
    }
}

fn health_label(core: &RyeOsCore) -> String {
    core.data
        .dimension
        .as_ref()
        .and_then(|dimension| dimension.local_node.health.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or("connecting")
        .to_string()
}

fn ryeos_version(core: &RyeOsCore) -> String {
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

fn tone_for_health(value: &str) -> RyeOsTone {
    let lower = value.to_ascii_lowercase();
    if lower.contains("healthy") {
        RyeOsTone::Good
    } else if lower.contains("degraded") {
        RyeOsTone::Warn
    } else if lower.contains("error") || lower.contains("failed") {
        RyeOsTone::Danger
    } else {
        RyeOsTone::Neutral
    }
}

fn subtitle(core: &RyeOsCore) -> String {
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
    use crate::ui::content::{ProjectedRecord, TimelineRole};
    use crate::ui::{RyeOsEvent, RyeOsUiEvent};
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
            vec![RyeOsTimelineEntryVm::Block {
                text: "hello world".to_string(),
                tone: RyeOsTone::Accent,
            }]
        );
    }

    #[test]
    fn empty_center_resolves_backdrop_scene_from_surface_ref() {
        let session = crate::ui::model::BrowserSession {
            session_id: "S-backdrop".to_string(),
            surface_ref: "surface:ryeos/ryeos/base".to_string(),
            effective_surface: Some(json!({
                "name": "ryeos-base",
                "version": "1.0.0",
                "backdrop": "view:test/backdrop",
                // The backdrop is content: its scene objects live in the
                // embedded view's body, not in Rust.
                "views": {
                    "view:test/backdrop": {
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
        let core = RyeOsCore::new(session, crate::ui::model::BrowserViewport::default(), 0);
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
    fn status_view_in_top_dock_includes_notice_rows() {
        let session = crate::ui::model::BrowserSession {
            session_id: "S-status".to_string(),
            surface_ref: "surface:ryeos/ryeos/base".to_string(),
            effective_surface: Some(json!({
                "name": "ryeos-base",
                "slots": {
                    "top": { "content": "view:ryeos/node/status", "open": true, "size": 3 }
                },
                "views": {
                    "view:ryeos/node/status": {
                        "widget": "key_value",
                        "source": { "ref": "service:node/status" },
                        "projections": { "detail": ["version"] }
                    }
                }
            })),
            ..Default::default()
        };
        let mut core = RyeOsCore::new(session, crate::ui::model::BrowserViewport::default(), 0);
        core.data
            .sources
            .insert("dock:top".to_string(), json!({ "version": "0.1.0" }));
        core.notice(
            "Queued behind active thread · 2 staged inputs.",
            RyeOsTone::Accent,
        );

        let vm = build_view_model(&core);
        let dock = vm
            .workspace
            .docks
            .top
            .expect("authored top status slot should render");
        assert_eq!(dock.edge, RyeOsDockEdge::Top);
        assert_eq!(dock.title, "status");
        assert_eq!(dock.size, 3);
        match dock.view {
            RyeOsViewVm::Rows { rows, .. } => {
                assert!(rows.iter().any(|row| row.primary == "version: 0.1.0"));
                let notice = rows
                    .iter()
                    .find(|row| row.primary == "Queued behind active thread · 2 staged inputs.")
                    .expect("notice should be part of the status view");
                assert_eq!(notice.meta.as_deref(), Some("notice"));
                assert_eq!(notice.tone, RyeOsTone::Accent);
                assert!(!notice.selected);
            }
            other => panic!("expected rows status dock, got {other:?}"),
        }
    }

    #[test]
    fn no_backdrop_ref_means_no_backdrop_scene() {
        let session = crate::ui::model::BrowserSession {
            session_id: "S-nobackdrop".to_string(),
            surface_ref: "surface:ryeos/ryeos/base".to_string(),
            effective_surface: Some(json!({ "name": "ryeos-base", "views": {} })),
            ..Default::default()
        };
        let core = RyeOsCore::new(session, crate::ui::model::BrowserViewport::default(), 0);
        let vm = build_view_model(&core);
        assert!(vm.workspace.center_is_empty);
        assert!(vm.workspace.backdrop.is_none());
    }

    fn session_with_views(views: serde_json::Value, tiles: serde_json::Value) -> RyeOsCore {
        let session = crate::ui::model::BrowserSession {
            session_id: "S-title".to_string(),
            surface_ref: "surface:ryeos/ryeos/base".to_string(),
            effective_surface: Some(json!({
                "name": "ryeos-base",
                "tiles": tiles,
                "views": views,
            })),
            ..Default::default()
        };
        RyeOsCore::new(session, crate::ui::model::BrowserViewport::default(), 0)
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
    fn view_overlay_labels_come_from_content_with_ref_fallback() {
        let core = session_with_views(
            json!({
                "view:ryeos/atlas": { "widget": "atlas", "name": "atlas-slug", "title": "Atlas", "description": "the namespace atlas" },
                "view:ryeos/x/raw": { "widget": "rows" },
            }),
            json!(["view:ryeos/atlas", "view:ryeos/x/raw"]),
        );
        let items = view_overlay_items(&core);
        let labels: Vec<&str> = items
            .iter()
            .filter(|item| !item.header)
            .map(|item| item.primary.as_str())
            .collect();
        assert!(
            labels.contains(&"Atlas"),
            "authored title labels the view: {labels:?}"
        );
        assert!(
            labels.contains(&"ryeos/x/raw"),
            "unnamed view falls back to stripped ref: {labels:?}"
        );
        // Every leaf sits under its group header, indented one level.
        assert!(items.iter().any(|item| item.header));
        assert!(items
            .iter()
            .filter(|item| !item.header)
            .all(|item| item.depth == 1));
    }

    #[test]
    fn surface_style_border_flows_into_presentation_chrome() {
        let session = crate::ui::model::BrowserSession {
            session_id: "S-border".to_string(),
            surface_ref: "surface:ryeos/ryeos/base".to_string(),
            effective_surface: Some(json!({
                "name": "ryeos-base",
                "style": { "border": "thick" }
            })),
            ..Default::default()
        };
        let core = RyeOsCore::new(session, crate::ui::model::BrowserViewport::default(), 0);
        let vm = build_view_model(&core);
        assert_eq!(vm.presentation.chrome.border, "thick");

        // Absent style defaults to thin.
        let default_vm = build_view_model(&RyeOsCore::default());
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
                RyeOsTimelineEntryVm::Pair {
                    summary: "run tool".to_string(),
                    meta: Some("42ms".to_string()),
                    tone: RyeOsTone::Good,
                    pending: false,
                },
                RyeOsTimelineEntryVm::Line {
                    primary: "thinking".to_string(),
                    meta: None,
                    tone: RyeOsTone::Neutral,
                    intent: None,
                    secondary_intent: None,
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
            vec![RyeOsTimelineEntryVm::Line {
                primary: "orphan close".to_string(),
                meta: Some("done".to_string()),
                tone: RyeOsTone::Good,
                intent: None,
                secondary_intent: None,
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
            vec![RyeOsTimelineEntryVm::Pair {
                summary: "run tool".to_string(),
                meta: None,
                tone: RyeOsTone::Accent,
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
            vec![RyeOsTimelineEntryVm::Separator {
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
            vec![RyeOsTimelineEntryVm::Line {
                primary: "message".to_string(),
                meta: None,
                tone: RyeOsTone::Neutral,
                intent: None,
                secondary_intent: None,
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
            &[RyeOsTimelineEntryVm::Block {
                text: "durable".into(),
                tone: RyeOsTone::Neutral,
            }]
        );
    }

    #[test]
    fn actual_chain_timeline_binding_projects_replay_shapes() {
        let binding: super::super::content::ViewBinding = serde_yaml::from_str(include_str!(
            "../../../../../bundles/ryeos-ui/.ai/views/ryeos/chain/timeline.yaml"
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
            |entry| matches!(entry, RyeOsTimelineEntryVm::Block { text, .. } if text == "answer")
        ));
        assert!(entries.iter().any(|entry| matches!(
            entry,
            RyeOsTimelineEntryVm::Separator { label } if label == "1"
        )));
        assert!(entries.iter().any(|entry| matches!(
            entry,
            RyeOsTimelineEntryVm::Pair { summary, tone: RyeOsTone::Good, pending: false, .. } if summary == "tool:demo"
        )));
        assert!(!entries.iter().any(|entry| matches!(
            entry,
            RyeOsTimelineEntryVm::Line { primary, .. } if primary.contains("live-only")
        )));
    }

    #[test]
    fn actual_threads_list_binding_projects_a_table() {
        use crate::ui::content::{project_table, table_columns, ViewBinding};
        let binding: ViewBinding = serde_yaml::from_str(include_str!(
            "../../../../../bundles/ryeos-ui/.ai/views/ryeos/threads/list.yaml"
        ))
        .unwrap();
        assert_eq!(binding.widget, "table");
        // The watch dashboard sources the operator-scoped UI-ryeos list,
        // active-first (sort: watch), before the limit.
        let source = binding.source.as_ref().expect("threads list has a source");
        assert_eq!(source.item_ref, "service:ui/ryeos-ui/threads/list");
        assert_eq!(source.params["limit"], 200);
        assert_eq!(source.params["sort"], "watch");
        assert_eq!(source.collection.as_deref(), Some("threads"));
        let columns = table_columns(&binding);
        assert_eq!(
            columns.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
            ["thread", "kind", "item", "status", "source", "follow", "created"]
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
            [
                "T-ab",
                "directive",
                "directive:ops/base",
                "running",
                "fp:claude",
                "",
                "2026-06-29T00:00:00Z"
            ]
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
            RyeOsTimelineEntryVm::Line { primary, tone, .. } => {
                assert!(primary.contains("running"), "{primary}");
                assert!(
                    primary.contains("1200") && primary.contains("340"),
                    "{primary}"
                );
                assert!(primary.contains("$0.0421"), "{primary}");
                assert!(primary.contains("3 turns"), "{primary}");
                assert_eq!(tone, RyeOsTone::Accent);
            }
            other => panic!("expected a header Line, got {other:?}"),
        }
        // A timeline whose source carries no `summary` gets no header.
        assert!(timeline_summary_entry(&json!({ "events": [] })).is_none());
    }

    #[test]
    fn status_tone_maps_by_typed_status_variant() {
        assert_eq!(status_tone("running"), RyeOsTone::Accent);
        assert_eq!(status_tone("created"), RyeOsTone::Accent);
        assert_eq!(status_tone("completed"), RyeOsTone::Good);
        assert_eq!(status_tone("continued"), RyeOsTone::Good);
        assert_eq!(status_tone("failed"), RyeOsTone::Danger);
        assert_eq!(status_tone("killed"), RyeOsTone::Danger);
        assert_eq!(status_tone("timed_out"), RyeOsTone::Danger);
        assert_eq!(status_tone("cancelled"), RyeOsTone::Warn);
        // An unrecognized status folds to Unknown → neutral, not a panic.
        assert_eq!(status_tone("some_future_status"), RyeOsTone::Neutral);
    }

    #[test]
    fn actual_thread_detail_binding_projects_inspect_sections() {
        use crate::ui::content::{project_section, ViewBinding};
        let binding: ViewBinding = serde_yaml::from_str(include_str!(
            "../../../../../bundles/ryeos-ui/.ai/views/ryeos/threads/detail.yaml"
        ))
        .unwrap();
        assert_eq!(binding.widget, "sections");
        // One inspect response feeds every section; each reads a different
        // sub-value of it (thread / result / artifacts / children).
        let response = json!({
            "schema_version": "ryeos.thread.inspect.v1",
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
        use crate::ui::content::{project_table, project_tone, table_columns, ViewBinding};
        let binding: ViewBinding = serde_yaml::from_str(include_str!(
            "../../../../../bundles/ryeos-ui/.ai/views/ryeos/items/space.yaml"
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
        use crate::ui::event::{RyeOsEvent, RyeOsUiEvent};
        use crate::ui::model::{BrowserSession, BrowserViewport};
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
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        let key = core.workspace.focused_tile.0.to_string();
        core.data.sources.insert(
            key.clone(),
            json!({ "threads": [
                { "thread_id": "T-ab", "status": "running" },
                { "thread_id": "T-cd", "status": "running" }
            ]}),
        );

        fn find_tile_view(node: &RyeOsLayoutNodeVm) -> Option<&RyeOsViewVm> {
            match node {
                RyeOsLayoutNodeVm::Tile { view, .. } => Some(view),
                RyeOsLayoutNodeVm::Split { first, second, .. } => {
                    find_tile_view(first).or_else(|| find_tile_view(second))
                }
            }
        }
        let selected_cells = |core: &RyeOsCore| -> Vec<Vec<String>> {
            let vm = build_view_model(core);
            let root = vm.workspace.root.expect("layout root");
            match find_tile_view(&root).expect("tile view") {
                RyeOsViewVm::Table { rows, .. } => rows
                    .iter()
                    .filter(|r| r.selected)
                    .map(|r| r.cells.clone())
                    .collect(),
                other => panic!("expected table view, got {other:?}"),
            }
        };

        // Flat cursor 1 = the second row; activation carries that row's record.
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetTileCursor {
                tile_id: key.clone(),
                index: 1,
            },
        });
        assert_eq!(
            selected_cells(&core),
            vec![vec!["T-cd".to_string(), "running".to_string()]]
        );
        match intent_for_focused_row(&core).expect("table row activates") {
            RyeOsUiIntent::InvokeAffordance {
                affordance_id,
                record,
                ..
            } => {
                assert_eq!(affordance_id, "inspect");
                assert_eq!(record["thread_id"], "T-cd");
            }
            other => panic!("expected inspect invoke, got {other:?}"),
        }
    }

    #[test]
    fn command_overlay_surfaces_focused_row_cancel_but_not_the_activate() {
        use crate::ui::model::{BrowserSession, BrowserViewport};
        let session = BrowserSession {
            effective_surface: Some(json!({
                "name": "t",
                "tiles": ["view:ryeos/threads/list"],
                "views": {
                    "view:ryeos/threads/list": {
                        "widget": "table",
                        "source": { "ref": "service:ui/ryeos-ui/threads/list", "collection": "threads" },
                        "projections": { "columns": [ { "label": "thread", "field": "thread_id" } ] },
                        "selection": { "activate": "watch" },
                        "affordances": [
                            { "id": "watch", "label": "Watch",
                              "invoke": { "plane": "ui", "facet": "input.route",
                                          "merge": { "thread": "{record.thread_id}" },
                                          "open_view": "view:ryeos/chain/timeline" } },
                            { "id": "cancel", "label": "Cancel",
                              "invoke": { "plane": "rye", "ref": "service:commands/submit",
                                          "args": { "thread_id": "{record.thread_id}", "command_type": "cancel" } } }
                        ]
                    }
                }
            })),
            read_only: false,
            ..Default::default()
        };
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        let key = core.workspace.focused_tile.0.to_string();
        core.data.sources.insert(
            key,
            json!({ "threads": [ { "thread_id": "T-ab", "chain_root_id": "T-ab" } ] }),
        );

        let items = command_overlay_items_for(&core);
        // The focused row (default cursor 0 = T-ab) exposes a reachable Cancel
        // targeting that specific row.
        let cancel = items
            .iter()
            .find(|i| i.label == "Cancel")
            .expect("cancel overlay item");
        assert!(
            matches!(&cancel.intent,
                RyeOsUiIntent::InvokeAffordance { view_ref, affordance_id, record }
                    if view_ref == "view:ryeos/threads/list"
                        && affordance_id == "cancel"
                        && record["thread_id"] == "T-ab"),
            "cancel item must invoke the row's cancel affordance; got {:?}",
            cancel.intent
        );
        // The activate (watch) affordance is NOT duplicated as a context item —
        // it's already the row's Enter intent.
        assert!(
            !items.iter().any(|i| i.label == "Watch"),
            "activate affordance should not be surfaced as a context item"
        );
    }

    /// Build a single focused timeline tile over a chain_replay response, with
    /// the feed point (distance-from-bottom 0) on the newest entry.
    fn feed_core(events: serde_json::Value) -> RyeOsCore {
        use crate::ui::model::{BrowserSession, BrowserViewport};
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
        let mut core = RyeOsCore::new(session, BrowserViewport::default(), 0);
        let key = core.workspace.focused_tile.0.to_string();
        core.data
            .sources
            .insert(key.clone(), json!({ "events": events }));
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetTileCursor {
                tile_id: key,
                index: 0,
            },
        });
        core
    }

    #[test]
    fn command_overlay_offers_inspect_and_retry_on_a_focused_failed_feed_entry() {
        let core = feed_core(json!([
            { "event_type": "cognition_in", "thread_id": "T-1", "payload": { "content": "do it" } },
            { "event_type": "thread_failed", "thread_id": "T-1", "chain_root_id": "R-1",
              "payload": { "error": { "message": "boom" } } }
        ]));

        let items = command_overlay_items_for(&core);
        // Enter=inspect, carrying the visible line as title and the raw event.
        let inspect = items
            .iter()
            .find(|i| i.label == "Inspect selection")
            .expect("inspect item on a failed entry");
        assert!(
            matches!(&inspect.intent, RyeOsUiIntent::InspectSummary { title, .. } if title == "failed — boom")
        );
        // Retry is the inspect item's Shift+Enter secondary …
        assert!(
            matches!(&inspect.secondary_intent,
                Some(RyeOsUiIntent::PrefillRetryTurn { thread_id, chain_root_id, input })
                    if thread_id == "T-1" && chain_root_id == "R-1" && input == "do it"),
            "retry is offered as the inspect item's secondary; got {:?}",
            inspect.secondary_intent
        );
        // … AND a distinct plain-Enter item (for clients that can't send Shift+Enter).
        let retry = items
            .iter()
            .find(|i| i.label == "Retry failed turn")
            .expect("distinct retry item");
        assert!(matches!(
            &retry.intent,
            RyeOsUiIntent::PrefillRetryTurn { thread_id, .. } if thread_id == "T-1"
        ));
    }

    #[test]
    fn command_overlay_offers_neither_inspect_nor_retry_on_a_cancelled_terminal() {
        // Cancelled is operator-initiated, not an error — it is neither
        // inspectable nor retryable.
        let core = feed_core(json!([
            { "event_type": "cognition_in", "thread_id": "T-1", "payload": { "content": "do it" } },
            { "event_type": "thread_cancelled", "thread_id": "T-1", "chain_root_id": "R-1",
              "payload": {} }
        ]));
        let items = command_overlay_items_for(&core);
        assert!(!items.iter().any(|i| i.label == "Retry failed turn"));
        assert!(!items.iter().any(|i| i.label == "Inspect selection"));
    }

    #[test]
    fn command_overlay_offers_inspect_but_not_retry_on_a_timed_out_terminal() {
        // timed_out is inspectable but not retryable in v1 (the daemon refuses
        // continuation for that status).
        let core = feed_core(json!([
            { "event_type": "cognition_in", "thread_id": "T-1", "payload": { "content": "do it" } },
            { "event_type": "thread_timed_out", "thread_id": "T-1", "chain_root_id": "R-1",
              "payload": {} }
        ]));
        let items = command_overlay_items_for(&core);
        assert!(items.iter().any(|i| i.label == "Inspect selection"));
        assert!(!items.iter().any(|i| i.label == "Retry failed turn"));
    }

    #[test]
    fn append_live_delta_adds_trailing_cursor_block_for_head_thread() {
        let mut core = RyeOsCore::default();
        core.seat
            .append_facet(crate::ui::seat::KEY_INPUT_ROUTE, json!({ "thread": "T-1" }));
        core.data.live_delta = Some(crate::ui::model::RyeOsLiveDelta {
            thread: "T-1".to_string(),
            text: "Hel".to_string(),
        });

        let mut entries = Vec::new();
        append_live_delta(&core, &mut entries);

        // Accent-toned trailing block with a cursor — the in-progress turn.
        assert!(matches!(
            entries.as_slice(),
            [RyeOsTimelineEntryVm::Block { text, tone: RyeOsTone::Accent }]
                if text == "Hel\u{258d}"
        ));
    }

    #[test]
    fn append_live_delta_ignores_buffer_for_non_head_thread() {
        let mut core = RyeOsCore::default();
        core.seat
            .append_facet(crate::ui::seat::KEY_INPUT_ROUTE, json!({ "thread": "T-1" }));
        // A buffer left over from a different head must not render.
        core.data.live_delta = Some(crate::ui::model::RyeOsLiveDelta {
            thread: "T-OTHER".to_string(),
            text: "stale".to_string(),
        });

        let mut entries = Vec::new();
        append_live_delta(&core, &mut entries);
        assert!(entries.is_empty());
    }

    #[test]
    fn append_live_delta_shows_working_indicator_when_head_runs_silently() {
        let mut core = RyeOsCore::default();
        core.seat
            .append_facet(crate::ui::seat::KEY_INPUT_ROUTE, json!({ "thread": "T-1" }));
        // Head thread is running but has emitted no streaming text yet.
        core.data.threads = Some(crate::ui::dto::RyeOsThreadsDto {
            threads: vec![json!({ "thread_id": "T-1", "status": "running" })],
        });

        let mut entries = Vec::new();
        append_live_delta(&core, &mut entries);
        assert!(
            matches!(
                entries.as_slice(),
                [RyeOsTimelineEntryVm::Line { primary, tone: RyeOsTone::Accent, .. }]
                    if primary.contains("working")
            ),
            "running head with no tail → working indicator: {entries:?}"
        );
    }

    #[test]
    fn append_live_delta_no_indicator_when_head_thread_settled() {
        let mut core = RyeOsCore::default();
        core.seat
            .append_facet(crate::ui::seat::KEY_INPUT_ROUTE, json!({ "thread": "T-1" }));
        core.data.threads = Some(crate::ui::dto::RyeOsThreadsDto {
            threads: vec![json!({ "thread_id": "T-1", "status": "completed" })],
        });

        let mut entries = Vec::new();
        append_live_delta(&core, &mut entries);
        assert!(entries.is_empty(), "settled head → no working indicator");
    }

    #[test]
    fn conversation_usage_reads_chain_summary_not_event_sums() {
        // `thread_usage` payloads are cumulative-so-far (100 → 105), so the
        // conversation total is the daemon's continuation-aware `summary`
        // block, never the events summed (that would read 205).
        let mut core = RyeOsCore::default();
        core.data.sources.insert(
            "timeline".to_string(),
            json!({
                "events": [
                    { "event_type": "thread_usage", "payload": { "input_tokens": 100, "output_tokens": 20 } },
                    { "event_type": "cognition_out", "payload": { "content": "hi" } },
                    { "event_type": "thread_usage", "payload": { "input_tokens": 105, "output_tokens": 23 } },
                ],
                "summary": { "status": "completed", "input_tokens": 105, "output_tokens": 23 },
            }),
        );
        assert_eq!(conversation_usage(&core), (105, 23));
    }

    #[test]
    fn conversation_usage_is_zero_without_usage_summary() {
        assert_eq!(conversation_usage(&RyeOsCore::default()), (0, 0));
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
            [RyeOsTimelineEntryVm::Line { primary, .. }] if primary == "message"
        ));
    }

    fn input_session(view: serde_json::Value) -> crate::ui::model::RyeOsCore {
        let session = crate::ui::model::BrowserSession {
            effective_surface: Some(json!({
                "name": "t",
                "slots": { "bottom": { "content": "view:ryeos/input", "open": true, "size": 7 } },
                "views": { "view:ryeos/input": view }
            })),
            ..Default::default()
        };
        RyeOsCore::new(session, crate::ui::model::BrowserViewport::default(), 0)
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
            crate::ui::content::completion_source_key("view:ryeos/input", "line"),
            json!({
                "commands": [
                    { "invocable": true, "tokens": ["thread", "list"], "description": "List threads" },
                    { "invocable": true, "tokens": ["thread", "get"], "description": "Get thread" }
                ]
            }),
        );
        core.ui.input_buffers.insert(
            crate::ui::model::InputBufferKey::new("dock:bottom", "view:ryeos/input", "line")
                .storage_key(),
            crate::ui::model::RyeOsInputState {
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
            crate::ui::content::completion_source_key("view:ryeos/input", "line"),
            json!({ "commands": [
                { "invocable": true, "tokens": ["thread", "list"] }
            ] }),
        );
        core.ui.input_buffers.insert(
            crate::ui::model::InputBufferKey::new("dock:bottom", "view:ryeos/input", "line")
                .storage_key(),
            crate::ui::model::RyeOsInputState {
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
            crate::ui::content::mention_source_key("view:ryeos/input", "line"),
            json!({ "threads": [
                { "thread_id": "T-ab", "item_ref": "directive:ops/base" },
                { "thread_id": "T-cd", "item_ref": "directive:demo/chat" }
            ]}),
        );
        core.ui.input_buffers.insert(
            crate::ui::model::InputBufferKey::new("dock:bottom", "view:ryeos/input", "line")
                .storage_key(),
            crate::ui::model::RyeOsInputState {
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
