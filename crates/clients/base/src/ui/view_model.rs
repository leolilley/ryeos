use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::content::ViewBinding;
use super::event::RyeOsUiIntent;
use super::event::{RyeOsTransportChannel, RyeOsTransportFreshness};
use super::model::{RyeOsCore, RyeOsDockEdge};
use super::scene_model::{RyeOsSceneModel, bind_scene_actions, build_scene_model};
use super::seat::InvokeTemplate;
use crate::ids::{RyeOsViewInstanceKey, TileId};
use crate::layout::{LayoutTree, SplitAxis};
use crate::surface::AmbientAtlasStyleSpec;
use crate::view_set::{TileState, ViewLocalState, ViewSpec};

mod dialogs;
mod execution;
mod navigation;
pub use dialogs::{RyeOsCommandChoice, RyeOsDialogVm, RyeOsShortcutEntryVm, RyeOsTileIntentVm};
pub use execution::RyeOsTimelineEntryVm;
#[cfg(test)]
use execution::status_tone;
pub(crate) use execution::timeline_summary_entry;
use execution::{facet_backed_response, focused_timeline_entry, retry_intent_for_focused_row};
pub use navigation::{
    RyeOsAmbientAtlasStyleVm, RyeOsAmbientAtlasVm, RyeOsAmbientModeVm, RyeOsAmbientVm,
    RyeOsNavigationItemVm, RyeOsNavigationVm, RyeOsSessionVm,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsViewModel {
    pub schema_version: String,
    pub generation: u64,
    #[serde(default)]
    pub now_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_chain_root_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_url: Option<String>,
    pub session: RyeOsSessionVm,
    pub navigation: RyeOsNavigationVm,
    pub chrome: RyeOsChromeVm,
    pub presentation: RyeOsPresentationVm,
    pub view_set: Option<RyeOsViewSetVm>,
    pub docks: RyeOsDockPlaneVm,
    pub backdrop: Option<RyeOsSceneModel>,
    pub dialogs: Vec<RyeOsDialogVm>,
    pub notices: Vec<RyeOsNoticeVm>,
    pub transport: RyeOsTransportVm,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effect_failures: Vec<RyeOsEffectFailureVm>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsTransportVm {
    pub freshness: RyeOsTransportFreshness,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_observed_at_ms: Option<u64>,
    pub channels: Vec<RyeOsTransportChannelVm>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsTransportChannelVm {
    pub channel: RyeOsTransportChannel,
    pub freshness: RyeOsTransportFreshness,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_observed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<super::effect::RyeOsUiError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsEffectFailureVm {
    pub effect_id: u64,
    pub error: super::effect::RyeOsUiError,
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
    pub tabs: Vec<RyeOsViewSetTabVm>,
    pub focused_title: String,
    pub layout_symbol: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsViewSetTabVm {
    pub view_set_id: crate::ids::ViewSetId,
    pub number: usize,
    pub title: String,
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
        view_set_number: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsViewSetVm {
    pub view_set_id: crate::ids::ViewSetId,
    pub layout_guard: String,
    pub split_min_ratio: f32,
    pub split_max_ratio: f32,
    /// Projection of the canonical editable tree. Inactive group members stay
    /// mounted but only the selected member's content is projected.
    pub root: Option<RyeOsLayoutNodeVm>,
    pub focused_tile: String,
    /// True when the center has no tiles. Drives backdrop-vs-tiles: when
    /// empty, the renderer draws `backdrop`; otherwise the layout tree.
    pub center_is_empty: bool,
    pub tile_count: usize,
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
    pub instance_key: RyeOsViewInstanceKey,
    pub edge: RyeOsDockEdge,
    pub title: String,
    pub attachment_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading: Option<RyeOsViewHeadingVm>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supplement: Option<RyeOsViewSupplementVm>,
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
    pub address: super::model::RyeOsInputAddress,
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
        group_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group_label: Option<String>,
        tabs: Vec<RyeOsViewTabVm>,
        instance_key: RyeOsViewInstanceKey,
        tile_id: String,
        focused: bool,
        maximized: bool,
        title: String,
        attachment_label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        heading: Option<RyeOsViewHeadingVm>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        supplement: Option<RyeOsViewSupplementVm>,
        intents: Vec<RyeOsTileIntentVm>,
        /// Boxed: the view VM dominates the enum's size next to `Split`.
        view: Box<RyeOsViewVm>,
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
pub struct RyeOsViewTabVm {
    pub tile_id: String,
    pub title: String,
    pub active: bool,
}

/// Authored introductory content, separate from a view's navigation label.
/// Uses the existing static `body` owner; never synthesizes task or worker facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RyeOsViewHeadingVm {
    pub title: String,
    #[serde(default)]
    pub eyebrow: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub metadata: Vec<String>,
}

fn view_heading(
    core: &RyeOsCore,
    instance: &RyeOsViewInstanceKey,
    view_ref: &str,
) -> Option<RyeOsViewHeadingVm> {
    let binding = core.binding_for_instance(instance, view_ref)?;
    let authored = binding.body.get("heading")?;
    Some(
        serde_json::from_value(authored.clone()).unwrap_or_else(|error| RyeOsViewHeadingVm {
            title: "Invalid authored heading".into(),
            eyebrow: String::new(),
            summary: error.to_string(),
            metadata: Vec::new(),
        }),
    )
}

/// Static authored material alongside a view's projected records. Scene
/// compilation reuses the existing scene owner. Nothing here grants actions
/// or claims a live task state; observations still belong to named sources.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RyeOsViewSupplementVm {
    #[serde(default)]
    pub frame_label: String,
    #[serde(default)]
    pub frame_detail: String,
    #[serde(default)]
    pub footer: String,
    #[serde(default)]
    pub footer_rows: Vec<RyeOsRowDetailVm>,
    #[serde(default)]
    pub excerpt_title: String,
    #[serde(default)]
    pub excerpt: Vec<RyeOsRowDetailVm>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<RyeOsSceneModel>,
}

fn view_supplement(
    core: &RyeOsCore,
    instance: &RyeOsViewInstanceKey,
    view_ref: &str,
) -> Option<RyeOsViewSupplementVm> {
    let body = &core.binding_for_instance(instance, view_ref)?.body;
    let value = body.get("supplement")?;
    let mut result: RyeOsViewSupplementVm =
        serde_json::from_value(value.clone()).unwrap_or_else(|error| RyeOsViewSupplementVm {
            footer: format!("Invalid authored supplement: {error}"),
            ..Default::default()
        });
    // A scene uses exactly the same content grammar as an existing scene view.
    if let Some(scene) = body.get("scene") {
        result.scene = Some(super::scene_model::scene_from_body(
            scene,
            core.scene_frame(),
        ));
    }
    Some(result)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RyeOsTableCoverageVm {
    pub state: String,
    pub reason: String,
}

fn table_source_coverage(binding: &ViewBinding, response: &Value) -> Option<RyeOsTableCoverageVm> {
    let path = binding.primary_source()?.1.coverage.as_deref()?;
    let report = super::content::field_path(response, path);
    let state = report
        .and_then(|value| value.get("state"))
        .and_then(Value::as_str)
        .filter(|state| {
            matches!(
                *state,
                "observed" | "complete" | "partial" | "failed" | "unavailable"
            )
        })
        .unwrap_or("unavailable");
    let reason = report
        .and_then(|value| value.get("reason"))
        .and_then(Value::as_str)
        .filter(|reason| {
            !reason.is_empty()
                && reason.len() <= 80
                && reason
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        })
        .unwrap_or("coverage_not_reported");
    Some(RyeOsTableCoverageVm {
        state: state.into(),
        reason: reason.into(),
    })
}

/// Observation status from the existing source owner, separate from the
/// service's collection coverage. Cached rows never establish current success.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsSourceStatusVm {
    pub state: String,
    pub message: String,
    pub stale: bool,
}

fn source_status(core: &RyeOsCore, key: &str) -> Option<RyeOsSourceStatusVm> {
    let cached = core.data.sources.contains_key(key);
    if core
        .data
        .source_unresolved
        .get(key)
        .copied()
        .unwrap_or(false)
    {
        return Some(RyeOsSourceStatusVm {
            state: "unresolved".into(),
            message: "Source subject or required context is unresolved".into(),
            stale: cached,
        });
    }
    if let Some(error) = core.data.source_errors.get(key) {
        return Some(RyeOsSourceStatusVm {
            state: "failed".into(),
            message: format!("{}: {}", error.code, error.message),
            stale: cached,
        });
    }
    let refreshing = core
        .data
        .source_epoch
        .get(key)
        .zip(core.data.source_stored_epoch.get(key))
        .is_some_and(|(requested, stored)| requested > stored);
    if refreshing || !cached {
        return Some(RyeOsSourceStatusVm {
            state: if cached { "refreshing" } else { "loading" }.into(),
            message: if cached {
                "Refreshing source"
            } else {
                "Waiting for source"
            }
            .into(),
            stale: cached,
        });
    }
    None
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RyeOsViewVm {
    Field {
        field: Box<super::field::RyeOsFieldVm>,
    },
    Text {
        title: String,
        lines: Vec<RyeOsTextLineVm>,
        position: RyeOsTextPositionVm,
    },
    /// Bounded textual document content projected from an admitted source.
    /// The view definition names the response fields; this primitive does not
    /// know about files, knowledge items or any other product-specific owner.
    Document {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_status: Option<RyeOsSourceStatusVm>,
        path: String,
        content: String,
        truncated: bool,
        provenance: String,
    },
    /// The generic content widget surface: every bound view renders
    /// through rows (typed widget variants arrive with the render pass).
    Rows {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_status: Option<RyeOsSourceStatusVm>,
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
        source_status: Option<RyeOsSourceStatusVm>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provenance: Option<String>,
        #[serde(default)]
        affordance_hints: Vec<String>,
        entries: Vec<RyeOsTimelineEntryVm>,
        /// Stable semantic identity per visible entry. Source-backed entries
        /// use their source event key; presentation-only entries use a digest
        /// of their semantic content.
        #[serde(default)]
        entry_ids: Vec<String>,
        /// The exact list cursor coordinate for each visible entry. Timeline
        /// cursors count visible entries from the bottom, unlike rows/tables.
        #[serde(default)]
        entry_cursors: Vec<usize>,
        #[serde(default)]
        entry_arrived_at_ms: Vec<Option<u64>>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_status: Option<RyeOsSourceStatusVm>,
        columns: Vec<String>,
        /// Total selectable rows in the source collection. `rows` may be a
        /// render window around the cursor for large lists.
        #[serde(default)]
        total_rows: usize,
        /// Service-reported collection coverage, separate from rendered row
        /// count. An absent or malformed report never implies completeness.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        coverage: Option<RyeOsTableCoverageVm>,
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
    RequiredSubject {
        title: String,
        input: String,
        facets: Vec<String>,
        actions: Vec<RyeOsTileIntentVm>,
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
    /// Exact cursor coordinate in the current complete projection. Render
    /// windows must never make their local array index look authoritative.
    #[serde(default)]
    pub cursor: usize,
    pub cells: Vec<String>,
    /// Structural position when the table declares `projections.hierarchy`.
    /// Renderers derive connector glyphs from this generic shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hierarchy: Option<RyeOsTableHierarchyVm>,
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
    /// The change's tone when it crossed a tone boundary (or the row just
    /// arrived) — renderers flash in this color instead of generic accent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_tone: Option<RyeOsTone>,
    /// The row's raw record — base-only (not serialized to clients). Overlays
    /// rebuilds the row's non-activate affordances (e.g. Cancel) from it, so row
    /// management is reachable. Skipped to avoid duplicating every record into
    /// the per-row wire payload.
    #[serde(skip)]
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsTableHierarchyVm {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestor_continues: Vec<bool>,
    pub is_last: bool,
    pub has_children: bool,
    pub collapsed: bool,
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
    /// Stable semantic identity of the authored section.
    pub id: String,
    /// Flat cursor coordinate of the section's header/first point.
    #[serde(default)]
    pub cursor: usize,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_status: Option<RyeOsSourceStatusVm>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsRowVm {
    pub id: String,
    /// Exact cursor coordinate in the current complete projection. Render
    /// windows must never make their local array index look authoritative.
    #[serde(default)]
    pub cursor: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glyph: Option<String>,
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
    /// The change's tone when it crossed a tone boundary (or the row just
    /// arrived) — renderers flash in this color instead of generic accent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_tone: Option<RyeOsTone>,
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
    let view_set = view_set_vm(core);
    let chrome = RyeOsChromeVm {
        title: "RyeOS".to_string(),
        subtitle: subtitle(core),
        health_label: health.clone(),
        health_tone: tone_for_health(&health),
    };
    let route = core.focused_input_route();
    let tail_url = route
        .chain_root
        .as_ref()
        .map(|root| format!("/ui/events/chain/{root}"));
    RyeOsViewModel {
        schema_version: "ryeos.ui.vm.v2".to_string(),
        generation: core.generation,
        now_ms: core.runtime.now_ms,
        tail_thread_id: route.thread,
        tail_chain_root_id: route.chain_root,
        tail_url,
        presentation: presentation_vm(core, &chrome, view_set.as_ref()),
        session,
        navigation: navigation_vm(core),
        chrome,
        view_set,
        docks: dock_plane_vm(core),
        backdrop: backdrop_scene(core),
        dialogs: mounted_dialogs(core),
        notices: core.notices_vm(),
        transport: transport_vm(core),
        effect_failures: effect_failures_vm(core),
    }
}

fn navigation_vm(core: &RyeOsCore) -> RyeOsNavigationVm {
    let destination = core
        .seat
        .fold()
        .get(super::seat::KEY_NAVIGATION_DESTINATION)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let focused = core
        .active_view_set()
        .and_then(|set| set.tiles.get(&set.focused_tile))
        .map(|tile| tile.view.view_ref.as_str());
    let items = core
        .surface_navigation()
        .into_iter()
        .map(|entry| {
            let selected = destination.as_deref() == Some(entry.id.as_str())
                || (destination.is_none() && focused == Some(entry.view.as_str()));
            RyeOsNavigationItemVm {
                id: entry.id,
                label: entry.label,
                selected,
                intent: RyeOsUiIntent::OpenView {
                    view: ViewSpec::bound(entry.view),
                },
            }
        })
        .collect();
    RyeOsNavigationVm { items }
}

fn transport_vm(core: &RyeOsCore) -> RyeOsTransportVm {
    RyeOsTransportVm {
        freshness: core.runtime.transport.overall_freshness(),
        last_observed_at_ms: core.runtime.transport.last_observed_at_ms(),
        channels: core
            .runtime
            .transport
            .channels
            .iter()
            .map(|(channel, state)| RyeOsTransportChannelVm {
                channel: *channel,
                freshness: state.freshness,
                last_observed_at_ms: state.last_observed_at_ms,
                error: state.error.clone(),
            })
            .collect(),
    }
}

fn effect_failures_vm(core: &RyeOsCore) -> Vec<RyeOsEffectFailureVm> {
    core.ui
        .effect_failures
        .iter()
        .map(|(effect_id, error)| RyeOsEffectFailureVm {
            effect_id: *effect_id,
            error: error.clone(),
        })
        .collect()
}

fn presentation_vm(
    core: &RyeOsCore,
    chrome: &RyeOsChromeVm,
    view_set: Option<&RyeOsViewSetVm>,
) -> RyeOsPresentationVm {
    let version = ryeos_version(core);
    // Chrome follows the current Surface's admitted declaration, not a cached
    // style from a different Surface or a tab's subject binding.
    let border = core
        .active_surface_attachment()
        .and_then(|attachment| attachment.presentation.definition.surface())
        .map(|surface| surface.style.border)
        .unwrap_or_default();
    RyeOsPresentationVm {
        schema_version: "ryeos.ui.presentation.v1".to_string(),
        theme: RyeOsThemeVm {
            id: "gruvbox-optic".to_string(),
            tone: RyeOsTone::Accent,
        },
        chrome: RyeOsPresentationChromeVm {
            title: "Rye OS".to_string(),
            version_label: format!("RYE OS - {version}"),
            border: border.name().to_string(),
            top_bar: top_bar_vm(core),
            status_bar: status_bar_vm(chrome, core),
        },
        metrics: presentation_metrics_vm(core, view_set),
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
        visible: core.active_surface().is_some()
            && (core.style.view_set_tabs || core.ui.top_status_visible),
        tabs: core
            .active_surface_view_sets()
            .unwrap_or(&[])
            .iter()
            .enumerate()
            .map(|(index, view_set)| RyeOsViewSetTabVm {
                view_set_id: view_set.id,
                number: index + 1,
                title: view_set.title.clone(),
                active: core
                    .active_view_set()
                    .is_some_and(|active| active.id == view_set.id),
                tile_count: view_set.tile_ids().len(),
            })
            .collect(),
        focused_title: focused_tile_title(core),
        layout_symbol: layout_symbol(core),
    }
}

fn focused_tile_title(core: &RyeOsCore) -> String {
    core.active_view_set()
        .and_then(|set| set.tiles.get(&set.focused_tile))
        .map(|tile| tile_title(core, tile))
        .unwrap_or_default()
}

/// Human labels are authored content, not item identifiers. Use the same
/// precedence for group tabs, slots and widget headings.
fn view_title<'a>(
    core: &'a RyeOsCore,
    instance: &RyeOsViewInstanceKey,
    view_ref: &'a str,
) -> &'a str {
    core.binding_for_instance(instance, view_ref)
        .and_then(|binding| {
            [binding.title.as_deref(), binding.name.as_deref()]
                .into_iter()
                .flatten()
                .map(str::trim)
                .find(|label| !label.is_empty())
        })
        .unwrap_or("Untitled view")
}

fn tile_title(core: &RyeOsCore, tile: &crate::view_set::TileState) -> String {
    view_title(core, &tile.instance_key, &tile.view.view_ref).to_owned()
}

fn layout_symbol(core: &RyeOsCore) -> String {
    let Some(view_set) = core.active_view_set() else {
        return String::new();
    };
    let total = view_set.tile_ids().len();
    let master = view_set.tiling.master.count.min(total);
    let slave = total.saturating_sub(master);
    format!("M{master}│S{slave}")
}

fn presentation_metrics_vm(
    core: &RyeOsCore,
    view_set: Option<&RyeOsViewSetVm>,
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
        view_set.map_or(0, |set| set.tile_count),
        core.ui.motion.len(),
        core.ui.loading.len(),
        active_thread_count,
        core.runtime.activity_pulse,
    );

    RyeOsPresentationMetricsVm {
        tile_count: view_set.map_or(0, |set| set.tile_count),
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

fn status_bar_vm(_chrome: &RyeOsChromeVm, core: &RyeOsCore) -> RyeOsStatusBarVm {
    let key_hint = if core
        .focused_view_instance_key()
        .and_then(|instance| core.retained_lens_frames.get(&instance))
        .is_none_or(|frames| frames.is_empty())
    {
        "ctrl+k open · alt+s shards · alt+t/b bars · ctrl+←/→ tab · ctrl+↑/↓ move".to_string()
    } else {
        "⌫ back · alt+← back · ctrl+k open · alt+s shards · alt+t/b bars · ctrl+←/→ tab · ctrl+↑/↓ move"
            .to_string()
    };
    // This is quiet interaction chrome, not a second diagnostics dashboard.
    // Root-session project and execution counters become actively misleading
    // once exact mounted attachments span projects.
    // Node health belongs to the top line; the foot remains interaction chrome.
    let segments = Vec::new();
    RyeOsStatusBarVm {
        visible: core.ui.bottom_status_visible,
        segments,
        key_hint,
        energy: core.runtime.activity_pulse.clamp(0.0, 1.0),
        attention: (core.runtime.now_ms < core.runtime.attention_until_ms)
            .then_some(RyeOsTone::Warn),
    }
}

fn view_set_vm(core: &RyeOsCore) -> Option<RyeOsViewSetVm> {
    let view_set = core.active_view_set()?;
    let center_is_empty = view_set.center_is_empty();
    Some(RyeOsViewSetVm {
        view_set_id: view_set.id,
        layout_guard: core.layout_guard(),
        split_min_ratio: crate::layout::MIN_SPLIT_RATIO,
        split_max_ratio: crate::layout::MAX_SPLIT_RATIO,
        root: view_set
            .presentation_layout()
            .map(|layout| layout_node_vm(&layout, core)),
        focused_tile: tile_id_text(view_set.focused_tile),
        center_is_empty,
        // The backdrop scene resolves for empty centers, and for populated
        // centers that opt into a translucent ambient underlay.
        tile_count: view_set.tile_ids().len(),
        lens_trail: core
            .focused_view_instance_key()
            .and_then(|instance| core.retained_lens_frames.get(&instance))
            .into_iter()
            .flatten()
            .map(|frame| {
                frame.label.clone().unwrap_or_else(|| {
                    core.binding_attachments
                        .get(&frame.binding.binding_attachment_id)
                        .filter(|retained| {
                            retained.descriptor.binding_generation
                                == frame.binding.binding_generation
                                && retained.descriptor.binding_digest
                                    == frame.binding.binding_digest
                        })
                        .and_then(|retained| retained.views.get(&frame.view_ref))
                        .and_then(|binding| binding.title.as_ref().or(binding.name.as_ref()))
                        .map(|title| title.trim())
                        .filter(|title| !title.is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(|| frame.view_ref.clone())
                })
            })
            .collect(),
        lens_label: core
            .focused_view_instance_key()
            .and_then(|instance| core.retained_lens_labels.get(&instance))
            .cloned()
            .flatten(),
    })
}

/// Resolve the backdrop scene from the surface's `backdrop` view ref.
/// Absent `backdrop` → no scene (the background fill stands).
fn backdrop_scene(core: &RyeOsCore) -> Option<RyeOsSceneModel> {
    let attachment_id = core
        .active_surface()
        .map(|surface| surface.binding().binding_attachment_id.as_str())
        .unwrap_or(core.root_attachment_id.as_str());
    let backdrop_ref = core
        .binding_attachment(attachment_id)
        .and_then(|attachment| attachment.presentation.definition.surface())
        .and_then(|surface| surface.backdrop.as_deref())?;
    // The backdrop is content: read the embedded backdrop VIEW's body and
    // build its scene generically. The surface names which view; the view
    // declares the objects. Swap the ref → swap the background, no Rust.
    let binding = core
        .binding_attachments
        .get(attachment_id)?
        .views
        .get(backdrop_ref)?;
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
    // busy view sets. Active threads should lift the scene slightly; transient
    // activity pulses carry the stronger "something just happened" signal.
    let active_component = ((active_threads.max(0) as f32) * 0.06).clamp(0.0, 0.24);
    scene.energy = active_component.max(core.runtime.activity_pulse.clamp(0.0, 1.0));
    Some(scene)
}

fn dock_plane_vm(core: &RyeOsCore) -> RyeOsDockPlaneVm {
    RyeOsDockPlaneVm {
        top: dock_tile_vm(
            core,
            RyeOsDockEdge::Top,
            core.presentation
                .as_ref()
                .and_then(|presentation| presentation.docks().get("top")),
        ),
        bottom: dock_tile_vm(
            core,
            RyeOsDockEdge::Bottom,
            core.presentation
                .as_ref()
                .and_then(|presentation| presentation.docks().get("bottom")),
        ),
        left: dock_tile_vm(
            core,
            RyeOsDockEdge::Left,
            core.presentation
                .as_ref()
                .and_then(|presentation| presentation.docks().get("left")),
        ),
        right: dock_tile_vm(
            core,
            RyeOsDockEdge::Right,
            core.presentation
                .as_ref()
                .and_then(|presentation| presentation.docks().get("right")),
        ),
    }
}

fn dock_tile_vm(
    core: &RyeOsCore,
    edge: RyeOsDockEdge,
    state: Option<&super::presentation_state::SharedDockMount>,
) -> Option<RyeOsDockTileVm> {
    let state = state?;
    if !state.visible {
        return None;
    }
    let view_ref = state.view().view_ref();
    let instance_key = state.view().instance_key();
    let focused = matches!(
        core.focus_target(),
        Some(super::model::RyeOsFocusTarget::MountedView { instance_key: focused }) if &focused == instance_key
    );
    Some(RyeOsDockTileVm {
        instance_key: instance_key.clone(),
        edge,
        title: view_title(core, instance_key, view_ref).to_owned(),
        attachment_label: selection_attachment_label(core, instance_key),
        heading: view_heading(core, instance_key, view_ref),
        supplement: view_supplement(core, instance_key, view_ref),
        size: state.size,
        focused,
        view: bound_view_vm_keyed(
            core,
            instance_key,
            None,
            dock_selected_state(core, instance_key),
            view_ref,
            &core.ui.atlas,
        ),
        input: instance_input_vm(core, instance_key, view_ref),
    })
}

/// Per-instance row-local UI state a bound view projects with — named
/// fields so the two BTreeSet-shaped members can't be transposed.
#[derive(Clone, Copy, Default)]
struct RowLocalState<'a> {
    cursor: Option<usize>,
    collapsed: Option<&'a std::collections::BTreeSet<usize>>,
    expanded_rows: Option<&'a std::collections::BTreeSet<String>>,
    collapsed_tree_rows: Option<&'a std::collections::BTreeSet<String>>,
    changed_rows: Option<&'a std::collections::BTreeMap<String, crate::view_set::RowFlash>>,
}

fn dock_selected_state<'a>(
    core: &'a RyeOsCore,
    instance_key: &RyeOsViewInstanceKey,
) -> RowLocalState<'a> {
    match core.mounted_local_state(instance_key) {
        Some(crate::view_set::ViewLocalState::GenericList {
            cursor,
            collapsed,
            expanded_rows,
            collapsed_tree_rows,
            changed_rows,
            ..
        }) => RowLocalState {
            cursor: Some(*cursor),
            collapsed: Some(collapsed),
            expanded_rows: Some(expanded_rows),
            collapsed_tree_rows: Some(collapsed_tree_rows),
            changed_rows: Some(changed_rows),
        },
        _ => RowLocalState::default(),
    }
}

/// The input prompt VM for a view instance, if its binding declares an
/// `input` block. Re-sources the active transient buffer; layout-neutral.
fn instance_input_vm(
    core: &RyeOsCore,
    instance_key: &RyeOsViewInstanceKey,
    view_ref: &str,
) -> Option<RyeOsInputVm> {
    let binding = core.binding_for_instance(instance_key, view_ref)?;
    let input = binding.input.as_ref()?;
    let key = core.input_key_for(instance_key.clone(), view_ref, input)?;
    input_vm(core, &key, view_ref, input)
}

/// Render a content-bound view: binding + source response -> widget VM.
/// Pure projection; unknown widgets and missing data degrade honestly.
pub(super) fn bound_view_vm(core: &RyeOsCore, tile_id: TileId, view_ref: &str) -> RyeOsViewVm {
    let Some(view_set) = core.active_view_set() else {
        return RyeOsViewVm::Placeholder {
            title: String::new(),
            message: "View set is no longer mounted".into(),
        };
    };
    let (expanded_rows, collapsed_tree_rows, changed_rows) = selected_row_state(core, tile_id);
    let Some(tile) = view_set.tiles.get(&tile_id) else {
        return RyeOsViewVm::Placeholder {
            title: String::new(),
            message: "View is no longer mounted".into(),
        };
    };
    let instance_key = tile.instance_key.clone();
    let scoped_dataset_key = tile_id.0.to_string();
    bound_view_vm_keyed(
        core,
        &instance_key,
        Some(&scoped_dataset_key),
        RowLocalState {
            cursor: selected_cursor(core, tile_id),
            collapsed: selected_collapsed(core, tile_id),
            expanded_rows,
            collapsed_tree_rows,
            changed_rows,
        },
        view_ref,
        core.tile_atlas_state(tile_id),
    )
}

fn bound_view_vm_keyed(
    core: &RyeOsCore,
    instance_key: &RyeOsViewInstanceKey,
    scoped_dataset_key: Option<&str>,
    local: RowLocalState<'_>,
    view_ref: &str,
    atlas: &crate::atlas::AtlasUiStateVm,
) -> RyeOsViewVm {
    let RowLocalState {
        cursor,
        collapsed,
        expanded_rows,
        collapsed_tree_rows,
        changed_rows,
    } = local;
    let Some(binding) = core.binding_for_instance(instance_key, view_ref) else {
        return RyeOsViewVm::Placeholder {
            title: view_ref.to_string(),
            message: format!("view {view_ref} is not embedded in the effective surface"),
        };
    };
    if let Some((input, facets)) = core.unresolved_required_subject(instance_key) {
        return RyeOsViewVm::RequiredSubject {
            title: "Subject required".into(),
            actions: required_subject_actions(core, instance_key, &facets),
            input,
            facets,
        };
    }
    // A binding that failed to parse/validate shows its reason, not its
    // (absent) content — honest degrade, not a silent "not embedded".
    if let Some(reason) = &binding.degraded {
        return RyeOsViewVm::Placeholder {
            title: view_ref.to_string(),
            message: reason.clone(),
        };
    }
    if binding.local_projection.is_some() {
        return super::local_projection::project(core, instance_key, binding);
    }
    let primary_source = binding.primary_source();
    let source_key = primary_source.map(|(channel, _)| {
        super::source_key::RyeOsSourceInstanceKey::named(instance_key.clone(), channel).encode()
    });
    // Engine scene widgets render from shared topology/item data + this
    // tile's atlas arrangement, not from a per-tile source response — so
    // they dispatch before the source-required arms below.
    match binding.widget.as_str() {
        "field" => {
            return RyeOsViewVm::Field {
                field: Box::new(project_field_vm(core, instance_key, view_ref, binding)),
            };
        }
        "atlas" => {
            // This tile's own scoped dataset when it has one (keyed by tile
            // id == source_key); otherwise None falls back to the shared data.
            let mut scene = super::scene_model::build_scene_model_for_instance(
                core,
                atlas,
                scoped_dataset_key.and_then(|key| core.data.tile_items.get(key)),
                scoped_dataset_key.and_then(|key| core.data.tile_file_space.get(key)),
                Some(instance_key),
            );
            let tile_id = instance_key.surface_tile_id().map(|id| id.0.to_string());
            bind_scene_actions(&mut scene, tile_id);
            return RyeOsViewVm::Atlas { scene };
        }
        "graph" => {
            // Graph renders shared topology; no per-tile content scope yet.
            let mut scene = super::scene_model::build_scene_model_for_instance(
                core,
                atlas,
                None,
                None,
                Some(instance_key),
            );
            let tile_id = instance_key.surface_tile_id().map(|id| id.0.to_string());
            bind_scene_actions(&mut scene, tile_id);
            return RyeOsViewVm::Map { scene };
        }
        "sections" => {
            // A sections view reads one typed source channel per section, not
            // the default response below, so it assembles here ahead of the
            // source-required arms.
            let title = view_title(core, instance_key, view_ref).to_owned();
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
                let section_cursor = flat;
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
                let key = super::source_key::RyeOsSourceInstanceKey::named(
                    instance_key.clone(),
                    &section.source_channel,
                )
                .encode();
                let records = core
                    .data
                    .sources
                    .get(&key)
                    .map(|response| super::content::project_section(section, response))
                    .unwrap_or_default();
                let expand_fields =
                    super::content::expand_fields_from_projection(&section.projection);
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
                        let row_key = format!(
                            "{index}:{}",
                            super::model::projected_row_key(
                                &record.raw,
                                rows.len(),
                                &section.projection,
                            )
                        );
                        let expanded =
                            expanded_rows.is_some_and(|expanded| expanded.contains(&row_key));
                        let selected = cursor == Some(flat);
                        if selected {
                            fold_section = Some(index);
                        }
                        flat += 1;
                        rows.push(RyeOsRowVm {
                            id: format!("{view_ref}#{row_key}"),
                            cursor: flat - 1,
                            glyph: section
                                .projection
                                .get("glyph")
                                .and_then(Value::as_str)
                                .and_then(|path| super::content::field_text(&record.raw, path)),
                            primary: record.primary,
                            secondary: section
                                .projection
                                .get("secondary")
                                .and_then(Value::as_str)
                                .and_then(|path| super::content::field_text(&record.raw, path)),
                            meta: record.meta,
                            kind: None,
                            intent: activate.and_then(|affordance_id| {
                                let affordance = binding.affordances.iter().find(|item| {
                                    item.get("id").and_then(serde_json::Value::as_str)
                                        == Some(affordance_id)
                                })?;
                                super::content::affordance_eligibility(affordance, &record.raw)
                                    .enabled
                                    .then(|| RyeOsUiIntent::InvokeAffordance {
                                        instance_key: instance_key.clone(),
                                        view_ref: view_ref.to_string(),
                                        affordance_id: affordance_id.clone(),
                                        record: record.raw.clone(),
                                    })
                            }),
                            tone: tone_from_name(record.tone.as_deref()),
                            selected,
                            expandable: !expand_fields.is_empty(),
                            expanded,
                            detail: if expanded {
                                detail_vm(&record.raw, &expand_fields)
                            } else {
                                Vec::new()
                            },
                            changed_at_ms: None,
                            changed_tone: None,
                        });
                    }
                }
                sections.push(RyeOsSectionVm {
                    id: section_identity(view_ref, section),
                    cursor: section_cursor,
                    title: section.title.clone(),
                    source_status: source_status(core, &key),
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
        .and_then(|facet| facet_backed_response(core, instance_key, facet));
    let response = facet_response.as_ref().or_else(|| {
        source_key
            .as_ref()
            .and_then(|key| core.data.sources.get(key))
    });
    let observation_status = if facet_response.is_some() {
        None
    } else {
        source_key
            .as_deref()
            .and_then(|key| source_status(core, key))
    };
    let title = view_title(core, instance_key, view_ref).to_owned();
    if binding.widget == "text"
        && let Some(lines) = static_text_lines(binding)
    {
        return RyeOsViewVm::Text {
            title,
            lines,
            position: text_position(binding),
        };
    }
    match (binding.widget.as_str(), response) {
        (_, None)
            if observation_status
                .as_ref()
                .is_some_and(|status| status.state == "unresolved" || status.state == "failed") =>
        {
            let status = observation_status.expect("status checked");
            RyeOsViewVm::Placeholder {
                title,
                message: format!("{}: {}", status.state, status.message),
            }
        }
        (_, None) => RyeOsViewVm::Placeholder {
            title,
            message: format!(
                "loading {} …",
                primary_source
                    .map(|(_, source)| source.item_ref.as_str())
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
                    let key =
                        super::model::projected_row_key(&record.raw, index, &binding.projections);
                    let expanded = expanded_rows.is_some_and(|set| set.contains(&key));
                    let detail = if expanded {
                        detail_vm(&record.raw, &expand_fields)
                    } else {
                        Default::default()
                    };
                    RyeOsRowVm {
                        id: format!("{view_ref}#{key}"),
                        cursor: index,
                        glyph: binding
                            .projections
                            .get("glyph")
                            .and_then(Value::as_str)
                            .and_then(|path| super::content::field_text(raw, path)),
                        primary: record.primary,
                        secondary: binding
                            .projections
                            .get("secondary")
                            .and_then(serde_json::Value::as_str)
                            .and_then(|path| super::content::field_text(raw, path)),
                        meta: record.meta,
                        kind: None,
                        intent: activate_affordance.as_ref().and_then(|affordance_id| {
                            let affordance = binding.affordances.iter().find(|item| {
                                item.get("id").and_then(serde_json::Value::as_str)
                                    == Some(affordance_id)
                            })?;
                            super::content::affordance_eligibility(affordance, &record.raw)
                                .enabled
                                .then(|| RyeOsUiIntent::InvokeAffordance {
                                    instance_key: instance_key.clone(),
                                    view_ref: view_ref.to_string(),
                                    affordance_id: affordance_id.clone(),
                                    record: record.raw.clone(),
                                })
                        }),
                        tone: tone_from_name(record.tone.as_deref()),
                        selected: selected_index == Some(index),
                        expandable: !expand_fields.is_empty(),
                        expanded,
                        detail,
                        changed_at_ms: changed_rows
                            .and_then(|rows| rows.get(&key))
                            .map(|flash| flash.at_ms),
                        changed_tone: changed_rows
                            .and_then(|rows| rows.get(&key))
                            .and_then(|flash| flash.tone.as_deref())
                            .map(|tone| tone_from_name(Some(tone))),
                    }
                })
                .collect();
            RyeOsViewVm::Rows {
                title,
                source_status: observation_status,
                columns: Vec::new(),
                total_rows,
                provenance: Some(view_ref.to_string()),
                affordance_hints: affordance_hints(binding),
                rows,
            }
        }
        ("timeline", Some(response)) => {
            use std::borrow::Cow;

            let cached = source_key
                .as_ref()
                .and_then(|key| core.data.timeline_sources.get(key));
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
                super::timeline::TimelineWindowInput {
                    entries: full.as_ref(),
                    indents: full_indents.as_ref(),
                    sources: full_sources.as_ref(),
                    sections: full_sections.as_ref(),
                    collapsible: collapsible.as_ref(),
                },
                super::timeline::live_delta_entry(core),
                collapsed.unwrap_or(&empty),
                cursor.unwrap_or(0),
                TIMELINE_RENDER_WINDOW,
            );
            let folded = windowed.folded;
            let selected = windowed.selected;
            let entry_cursors = windowed.cursors;
            let arrival_by_key: std::collections::HashMap<&str, Option<u64>> = cached
                .map(|cache| {
                    cache
                        .sources
                        .iter()
                        .zip(cache.arrivals.iter())
                        .filter_map(|(source, arrived)| {
                            Some((source.as_ref()?.key.as_str(), *arrived))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let entry_arrived_at_ms = folded
                .sources
                .iter()
                .map(|source| {
                    source.as_ref().and_then(|source| {
                        arrival_by_key.get(source.key.as_str()).copied().flatten()
                    })
                })
                .collect();
            let entry_ids = folded
                .entries
                .iter()
                .zip(folded.sources.iter())
                .zip(folded.sections.iter())
                .map(|((entry, source), section)| {
                    source.as_ref().map_or_else(
                        || {
                            let semantic = serde_json::to_vec(&(entry, section))
                                .expect("timeline presentation identity is serializable");
                            format!("timeline-entry:{:x}", Sha256::digest(semantic))
                        },
                        |source| format!("timeline-entry:{}", source.key),
                    )
                })
                .collect();
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
                source_status: observation_status,
                provenance: Some(view_ref.to_string()),
                affordance_hints: affordance_hints(binding),
                entries: folded.entries,
                entry_ids,
                entry_cursors,
                entry_arrived_at_ms,
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
            let empty_tree_folds = std::collections::BTreeSet::new();
            let visible_rows = super::content::table_hierarchy_rows(
                binding,
                records,
                collapsed_tree_rows.unwrap_or(&empty_tree_folds),
            )
            .map(|rows| {
                rows.into_iter()
                    .map(|row| (row.source_index, Some(row)))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| (0..records.len()).map(|index| (index, None)).collect());
            let total_rows = visible_rows.len();
            let (start, end) = row_render_window(total_rows, cursor);
            let selected_index = clamped_row_cursor(total_rows, cursor);
            let rows = visible_rows[start..end]
                .iter()
                .enumerate()
                .map(|(offset, (source_index, hierarchy))| {
                    let index = start + offset;
                    let raw = &records[*source_index];
                    let record = super::content::project_table_record(binding, raw, &columns);
                    let key = hierarchy
                        .as_ref()
                        .map(|tree| tree.key.clone())
                        .unwrap_or_else(|| {
                            super::model::projected_row_key(
                                &record.raw,
                                *source_index,
                                &binding.projections,
                            )
                        });
                    let expanded = expanded_rows.is_some_and(|set| set.contains(&key));
                    let detail = if expanded {
                        detail_vm(&record.raw, &expand_fields)
                    } else {
                        Default::default()
                    };
                    RyeOsTableRowVm {
                        id: format!("{view_ref}#{key}"),
                        cursor: index,
                        cells: record.cells,
                        hierarchy: hierarchy.as_ref().map(|tree| RyeOsTableHierarchyVm {
                            ancestor_continues: tree.ancestor_continues.clone(),
                            is_last: tree.is_last,
                            has_children: tree.has_children,
                            collapsed: tree.collapsed,
                        }),
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
                        intent: activate_affordance.as_ref().and_then(|affordance_id| {
                            let affordance = binding.affordances.iter().find(|item| {
                                item.get("id").and_then(serde_json::Value::as_str)
                                    == Some(affordance_id)
                            })?;
                            super::content::affordance_eligibility(affordance, &record.raw)
                                .enabled
                                .then(|| RyeOsUiIntent::InvokeAffordance {
                                    instance_key: instance_key.clone(),
                                    view_ref: view_ref.to_string(),
                                    affordance_id: affordance_id.clone(),
                                    record: record.raw.clone(),
                                })
                        }),
                        selected: selected_index == Some(index),
                        expandable: !expand_fields.is_empty(),
                        expanded,
                        detail,
                        changed_at_ms: changed_rows
                            .and_then(|rows| rows.get(&key))
                            .map(|flash| flash.at_ms),
                        changed_tone: changed_rows
                            .and_then(|rows| rows.get(&key))
                            .and_then(|flash| flash.tone.as_deref())
                            .map(|tone| tone_from_name(Some(tone))),
                        raw: record.raw,
                    }
                })
                .collect();
            RyeOsViewVm::Table {
                title,
                source_status: observation_status,
                columns: column_labels,
                total_rows,
                coverage: table_source_coverage(binding, response),
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
                    cursor: index,
                    glyph: None,
                    primary: format!("{key}: {value}"),
                    secondary: None,
                    meta: None,
                    kind: (binding.widget == "key_value").then(|| "detail".to_string()),
                    intent: None,
                    tone: RyeOsTone::Neutral,
                    selected: cursor == Some(index),
                    expandable: false,
                    expanded: false,
                    detail: if binding.widget == "key_value" {
                        vec![RyeOsRowDetailVm {
                            field: key,
                            value,
                            tone: None,
                        }]
                    } else {
                        Vec::new()
                    },
                    changed_at_ms: None,
                    changed_tone: None,
                })
                .collect();
            let notice_start = rows.len();
            rows.extend(status_notice_rows(core, view_ref, notice_start, cursor));
            RyeOsViewVm::Rows {
                title,
                source_status: observation_status,
                columns: Vec::new(),
                total_rows: rows.len(),
                provenance: Some(view_ref.to_string()),
                affordance_hints: affordance_hints(binding),
                rows,
            }
        }
        ("document", Some(response)) => {
            let projection = |name: &str| {
                binding
                    .projections
                    .get(name)
                    .and_then(serde_json::Value::as_str)
                    .and_then(|path| super::content::field_path(response, path))
            };
            let Some(path) = projection("path").and_then(serde_json::Value::as_str) else {
                return RyeOsViewVm::Placeholder {
                    title,
                    message: "document view has no projected path".to_string(),
                };
            };
            let Some(content) = projection("content").and_then(serde_json::Value::as_str) else {
                return RyeOsViewVm::Placeholder {
                    title,
                    message: "document view has no projected content".to_string(),
                };
            };
            RyeOsViewVm::Document {
                title,
                source_status: observation_status,
                path: path.to_string(),
                content: content.to_string(),
                truncated: projection("truncated")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                provenance: view_ref.to_string(),
            }
        }
        (other, Some(_)) => RyeOsViewVm::Placeholder {
            title,
            // Degradation: an unknown widget never crashes a renderer.
            message: format!("widget `{other}` is not supported by this renderer"),
        },
    }
}

fn field_local_state<'a>(
    core: &'a RyeOsCore,
    instance_key: &RyeOsViewInstanceKey,
) -> Option<&'a crate::view_set::FieldLocalState> {
    let local = core.mounted_local_state(instance_key);
    match local {
        Some(ViewLocalState::Field(local)) => Some(local),
        _ => None,
    }
}

fn project_field_vm(
    core: &RyeOsCore,
    instance_key: &RyeOsViewInstanceKey,
    view_ref: &str,
    binding: &super::content::ViewBinding,
) -> super::field::RyeOsFieldVm {
    let sources = binding
        .sources
        .iter()
        .map(|(channel, source)| {
            let key =
                super::source_key::RyeOsSourceInstanceKey::named(instance_key.clone(), channel)
                    .encode();
            let refreshing = core
                .data
                .source_epoch
                .get(&key)
                .zip(core.data.source_stored_epoch.get(&key))
                .is_some_and(|(requested, stored)| requested > stored);
            super::field::FieldSourceInput {
                channel,
                source_ref: &source.item_ref,
                subject_fingerprint: core
                    .data
                    .source_subject_fingerprint
                    .get(&key)
                    .map(String::as_str),
                response: core.data.sources.get(&key),
                parsed: core.data.field_sources.get(&key),
                error: core
                    .data
                    .source_errors
                    .get(&key)
                    .map(|error| error.message.as_str()),
                unresolved: core
                    .data
                    .source_unresolved
                    .get(&key)
                    .copied()
                    .unwrap_or(false),
                refreshing,
            }
        })
        .collect::<Vec<_>>();
    let local = field_local_state(core, instance_key);
    let title = binding
        .title
        .as_deref()
        .or(binding.name.as_deref())
        .unwrap_or_else(|| view_ref.rsplit('/').next().unwrap_or(view_ref));
    let field_id = format!("{}@{}", view_ref, instance_key);
    let fingerprint =
        super::field::field_projection_fingerprint(&field_id, title, view_ref, binding, &sources);
    let cached = core
        .data
        .field_projections
        .borrow()
        .get(instance_key)
        .filter(|entry| entry.fingerprint == fingerprint)
        .map(|entry| entry.base.clone());
    let base = cached.unwrap_or_else(|| {
        let base = super::field::project_field_for_instance(
            &field_id,
            title,
            view_ref,
            binding,
            &sources,
            None,
            instance_key,
        );
        core.data.field_projections.borrow_mut().insert(
            instance_key.clone(),
            super::field::FieldProjectionCacheEntry {
                fingerprint,
                base: base.clone(),
            },
        );
        base
    });
    super::field::apply_field_local_state(base, local)
}

pub(crate) fn field_vm_for_instance(
    core: &RyeOsCore,
    instance_key: &RyeOsViewInstanceKey,
) -> Option<(String, super::field::RyeOsFieldVm)> {
    let view_ref = core.mounted_view_ref(instance_key)?.to_owned();
    let binding = core.binding_for_instance(instance_key, &view_ref)?;
    (binding.widget == "field").then(|| {
        (
            view_ref.clone(),
            project_field_vm(core, instance_key, &view_ref, binding),
        )
    })
}

/// Project the exact currently mounted tile-or-dock view instance. Pointer
/// reducers use this to validate semantic item identity against current Rust
/// state rather than trusting a browser frame's position or intent payload.
pub(crate) fn view_vm_for_instance(
    core: &RyeOsCore,
    instance_key: &RyeOsViewInstanceKey,
) -> Option<RyeOsViewVm> {
    let mount = core.presentation.as_ref()?.mount(instance_key)?;
    if !mount.visible() {
        return None;
    }
    match mount {
        super::presentation_state::MountRef::SurfaceTile { tile_id, tile, .. } => {
            Some(bound_view_vm(core, tile_id, &tile.view.view_ref))
        }
        _ => Some(bound_view_vm_keyed(
            core,
            instance_key,
            None,
            dock_selected_state(core, instance_key),
            mount.view_ref(),
            &core.ui.atlas,
        )),
    }
}

pub(crate) fn view_section(
    core: &RyeOsCore,
    instance_key: &RyeOsViewInstanceKey,
    section_id: &str,
) -> Option<(usize, usize, bool)> {
    let RyeOsViewVm::Sections { sections, .. } = view_vm_for_instance(core, instance_key)? else {
        return None;
    };
    sections
        .into_iter()
        .enumerate()
        .find(|(_, section)| section.id == section_id)
        .map(|(index, section)| (index, section.cursor, section.collapsed))
}

fn section_identity(view_ref: &str, section: &super::content::SectionBinding) -> String {
    let semantic = serde_json::to_vec(&(
        view_ref,
        &section.title,
        &section.source_channel,
        &section.collection,
        &section.projection,
        &section.activate,
    ))
    .expect("section identity is serializable");
    format!("view-section:{:x}", Sha256::digest(semantic))
}

/// Resolve one semantic pointer target to its current cursor coordinate and
/// current intent. Only the reducer calls this; clients never carry intents
/// across frames for activation.
pub(crate) fn view_pointer_item(
    core: &RyeOsCore,
    instance_key: &RyeOsViewInstanceKey,
    item_id: &str,
) -> Option<(usize, Option<RyeOsUiIntent>)> {
    match view_vm_for_instance(core, instance_key)? {
        RyeOsViewVm::Rows { rows, .. } => rows
            .into_iter()
            .find(|row| row.id == item_id)
            .map(|row| (row.cursor, row.intent)),
        RyeOsViewVm::Table { rows, .. } => rows
            .into_iter()
            .find(|row| row.id == item_id)
            .map(|row| (row.cursor, row.intent)),
        RyeOsViewVm::Sections { sections, .. } => sections
            .into_iter()
            .flat_map(|section| section.rows)
            .find(|row| row.id == item_id)
            .map(|row| (row.cursor, row.intent)),
        RyeOsViewVm::Timeline {
            entries,
            entry_ids,
            entry_cursors,
            ..
        } => entry_ids
            .into_iter()
            .position(|id| id == item_id)
            .and_then(|index| {
                let cursor = *entry_cursors.get(index)?;
                let intent = match entries.get(index) {
                    Some(RyeOsTimelineEntryVm::Line { intent, .. }) => intent.clone(),
                    _ => None,
                };
                Some((cursor, intent))
            }),
        _ => None,
    }
}

/// Resolve an expandable semantic pointer target to its current cursor and
/// disclosure state. Non-expandable replacements fail closed even when they
/// reuse an old item id.
pub(crate) fn view_pointer_expansion(
    core: &RyeOsCore,
    instance_key: &RyeOsViewInstanceKey,
    item_id: &str,
) -> Option<(usize, bool, String)> {
    match view_vm_for_instance(core, instance_key)? {
        RyeOsViewVm::Rows { rows, .. } => rows
            .into_iter()
            .find(|row| row.id == item_id && row.expandable)
            .and_then(|row| {
                let key = row.id.rsplit_once('#')?.1.to_owned();
                Some((row.cursor, row.expanded, key))
            }),
        RyeOsViewVm::Table { rows, .. } => rows
            .into_iter()
            .find(|row| row.id == item_id && row.expandable)
            .and_then(|row| {
                let key = row.id.rsplit_once('#')?.1.to_owned();
                Some((row.cursor, row.expanded, key))
            }),
        RyeOsViewVm::Sections { sections, .. } => sections
            .into_iter()
            .flat_map(|section| section.rows)
            .find(|row| row.id == item_id && row.expandable)
            .and_then(|row| {
                let key = row.id.rsplit_once('#')?.1.to_owned();
                Some((row.cursor, row.expanded, key))
            }),
        RyeOsViewVm::Timeline {
            entry_ids,
            entry_cursors,
            entry_expandable,
            entry_expanded,
            ..
        } => entry_ids
            .iter()
            .position(|id| id == item_id)
            .filter(|index| entry_expandable.get(*index).copied().unwrap_or(false))
            .and_then(|index| {
                Some((
                    *entry_cursors.get(index)?,
                    entry_expanded.get(index).copied().unwrap_or(false),
                    entry_ids
                        .get(index)?
                        .strip_prefix("timeline-entry:")?
                        .to_owned(),
                ))
            }),
        _ => None,
    }
}

/// Resolve a branch in the exact visible mount. Leaves, replaced bindings,
/// removed rows, and hidden descendants cannot acquire a fold from stale UI.
pub(crate) fn view_pointer_tree_fold(
    core: &RyeOsCore,
    instance_key: &RyeOsViewInstanceKey,
    item_id: &str,
) -> Option<(usize, bool, String)> {
    let view_ref = core.mounted_view_ref(instance_key)?;
    let prefix = format!("{view_ref}#");
    let RyeOsViewVm::Table { rows, .. } = view_vm_for_instance(core, instance_key)? else {
        return None;
    };
    let row = rows.into_iter().find(|row| row.id == item_id)?;
    let hierarchy = row.hierarchy.as_ref()?;
    if !hierarchy.has_children {
        return None;
    }
    let key = row.id.strip_prefix(&prefix)?;
    let binding = core.binding_for_instance(instance_key, view_ref)?;
    let tree = super::content::table_hierarchy(binding)?;
    let (channel, _) = binding.primary_source()?;
    let source =
        super::source_key::RyeOsSourceInstanceKey::named(instance_key.clone(), channel).encode();
    let records = super::content::source_collection(binding, core.data.sources.get(&source)?);
    // Rendering may provide a positional fallback for malformed source rows;
    // that fallback cannot establish a stable identity for a delayed event.
    if records
        .iter()
        .filter(|record| super::content::field_text(record, &tree.id).as_deref() == Some(key))
        .count()
        != 1
    {
        return None;
    }
    Some((row.cursor, hierarchy.collapsed, key.to_owned()))
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
        .and_then(|selection| selection.activate.clone())
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
            glyph: None,
            id: format!("{view_ref}#{}", notice.id),
            cursor: start_index + offset,
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
            changed_tone: None,
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
) -> Option<RyeOsInputVm> {
    let address = core.input_address_for(key.clone())?;
    let buffer = core.input_buffers.get(&key.storage_key());
    let text = buffer.map(|b| b.text.clone()).unwrap_or_default();
    let cursor = buffer.map(|b| b.cursor).unwrap_or(0);
    let focused = core
        .focused_input_instance()
        .as_ref()
        .is_some_and(|(focused_key, _)| focused_key.storage_key() == key.storage_key());

    let route = core.input_route_for(key);
    let route_label = input
        .target_label
        .clone()
        .map(|label| format!("→ {label}"))
        .unwrap_or_else(|| {
            derived_target_label(core, &key.view_instance_key, view_ref, input, &route)
        });

    // Completion suggestions: an inline @-mention hint when the cursor is in
    // one, else the input's `/` command grammar.
    let completion = input_completion(core, key, input, &text, cursor);
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
    Some(RyeOsInputVm {
        address,
        focused,
        cursor,
        route_label,
        placeholder,
        hint,
        submit_enabled: !text.trim().is_empty() && (has_route_target || has_affordance_target),
        completion,
        live_filter,
        text,
    })
}

/// Derive the target strip when the author gives no `target_label`.
fn derived_target_label(
    core: &RyeOsCore,
    instance: &RyeOsViewInstanceKey,
    view_ref: &str,
    input: &super::content::InputBlock,
    route: &super::seat::InputRoute,
) -> String {
    if input.local_filter {
        return "Filter presentation actions".to_string();
    }
    if let Some(affordance_id) = input.submit_affordance() {
        // Derive from the bound affordance's invoke target.
        if let Some(target) = core
            .binding_for_instance(instance, view_ref)
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
    key: &super::model::InputBufferKey,
    input: &super::content::InputBlock,
    text: &str,
    cursor: usize,
) -> Vec<String> {
    // Inline @-mention: when the cursor is in an @-token, the hint lists
    // matching refs from the declared mentions source.
    if let Some(mentions) = input.mentions.as_ref()
        && super::tokenize::active_mention(text, cursor).is_some()
    {
        let records = core
            .data
            .sources
            .get(
                &super::source_key::RyeOsSourceInstanceKey::mention(
                    key.view_instance_key.clone(),
                    &key.input_id,
                )
                .encode(),
            )
            .map(|response| super::content::project_mentions(mentions, response))
            .unwrap_or_default();
        return super::tokenize::mention_hint(&records, text, cursor)
            .into_iter()
            .collect();
    }
    let Some(completion) = input.completion.as_ref() else {
        return Vec::new();
    };
    // The completion grammar is fetched through the generic keyed source path
    // (identical to mentions), keyed per (view_ref, input.id).
    let Some(response) = core.data.sources.get(
        &super::source_key::RyeOsSourceInstanceKey::completion(
            key.view_instance_key.clone(),
            &key.input_id,
        )
        .encode(),
    ) else {
        return Vec::new();
    };
    let records = super::content::completion_records(completion, response);
    super::tokenize::slash_completion_hint(records, text)
        .into_iter()
        .collect()
}

fn layout_node_vm(node: &LayoutTree, core: &RyeOsCore) -> RyeOsLayoutNodeVm {
    let Some(view_set) = core.active_view_set() else {
        unreachable!("layout projection requires an actual active view set");
    };
    match node {
        LayoutTree::Group {
            group_id,
            label,
            tabs,
            active: tile_id,
        } => {
            let view = view_set
                .tiles
                .get(tile_id)
                .map(|tile| view_vm(core, *tile_id, tile))
                .unwrap_or_else(|| RyeOsViewVm::Placeholder {
                    title: "Missing view".to_string(),
                    message: format!("Tile {} is not present in the view set.", tile_id.0),
                });
            let title = view_set
                .tiles
                .get(tile_id)
                .map(|tile| tile_title(core, tile))
                .unwrap_or_else(|| "Missing".to_string());
            let input = view_set
                .tiles
                .get(tile_id)
                .and_then(|tile| instance_input_vm(core, &tile.instance_key, &tile.view.view_ref));
            let chrome_hidden = view_set.tiles.get(tile_id).is_some_and(|tile| {
                view_hides_tile_chrome(core, &tile.instance_key, &tile.view.view_ref)
            });
            let background_transparent = view_set.tiles.get(tile_id).is_some_and(|tile| {
                view_has_transparent_background(core, &tile.instance_key, &tile.view.view_ref)
            });
            RyeOsLayoutNodeVm::Tile {
                group_id: group_id.0.to_string(),
                group_label: label.clone(),
                tabs: tabs
                    .iter()
                    .map(|id| RyeOsViewTabVm {
                        tile_id: tile_id_text(*id),
                        title: view_set
                            .tiles
                            .get(id)
                            .map(|tile| tile_title(core, tile))
                            .unwrap_or_else(|| "Missing view".to_string()),
                        active: id == tile_id,
                    })
                    .collect(),
                instance_key: view_set
                    .tiles
                    .get(tile_id)
                    .map(|tile| tile.instance_key.clone())
                    .expect("validated presentation layout names a mounted tile"),
                tile_id: tile_id_text(*tile_id),
                focused: *tile_id == view_set.focused_tile,
                maximized: view_set.maximized_tile == Some(*tile_id),
                title,
                attachment_label: view_set
                    .tiles
                    .get(tile_id)
                    .and_then(|tile| selection_attachment_label(core, &tile.instance_key)),
                heading: view_set
                    .tiles
                    .get(tile_id)
                    .and_then(|tile| view_heading(core, &tile.instance_key, &tile.view.view_ref)),
                supplement: view_set.tiles.get(tile_id).and_then(|tile| {
                    view_supplement(core, &tile.instance_key, &tile.view.view_ref)
                }),
                intents: tile_intents(core, *tile_id),
                view: Box::new(view),
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

fn view_hides_tile_chrome(
    core: &RyeOsCore,
    instance: &RyeOsViewInstanceKey,
    view_ref: &str,
) -> bool {
    core.binding_for_instance(instance, view_ref)
        .and_then(|binding| binding.presentation.chrome)
        .is_some_and(|chrome| matches!(chrome, super::content::ViewChromePresentation::None))
}

fn view_has_transparent_background(
    core: &RyeOsCore,
    instance: &RyeOsViewInstanceKey,
    view_ref: &str,
) -> bool {
    core.binding_for_instance(instance, view_ref)
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
    let surface = core.binding_attachment(&core.root_attachment_id);
    RyeOsSessionVm {
        session_id: browser
            .map(|session| session.session_id.clone())
            .unwrap_or_default(),
        project_path: surface.and_then(|attachment| attachment.project_path.clone()),
        root_ref: surface
            .map(|attachment| attachment.root_ref.clone())
            .unwrap_or_default(),
        title: core
            .active_surface()
            .map(|surface| surface.title().to_owned())
            .or_else(|| {
                surface.map(|attachment| attachment.presentation.definition.title().to_owned())
            })
            .unwrap_or_default(),
        surface_ordinal: core.active_surface().map(|surface| surface.ordinal()),
        surfaces: core
            .presentation
            .iter()
            .flat_map(|p| p.ordered_surfaces())
            .map(|surface| navigation::RyeOsSurfaceChoiceVm {
                surface_instance_id: surface.id().clone(),
                ordinal: surface.ordinal(),
                title: surface.title().to_owned(),
                active: core
                    .active_surface()
                    .is_some_and(|active| active.id() == surface.id()),
                select_intent: RyeOsUiIntent::SelectSurface {
                    surface_instance_id: surface.id().clone(),
                    binding: surface.binding().clone(),
                },
                rename_intent: core.composition_write_available().then(|| {
                    RyeOsUiIntent::RenameSurface {
                        surface_instance_id: surface.id().clone(),
                        binding: surface.binding().clone(),
                        title: surface.title().to_owned(),
                    }
                }),
                close_intent: core.surface_close_available(surface.id()).then(|| {
                    RyeOsUiIntent::CloseSurface {
                        surface_instance_id: surface.id().clone(),
                        binding: surface.binding().clone(),
                    }
                }),
            })
            .collect(),
        ambient: ambient_vm(core),
        user_principal_id: browser.and_then(|session| session.user_principal_id.clone()),
        posture: surface
            .map(|attachment| attachment.posture)
            .unwrap_or_default(),
    }
}

fn ambient_vm(core: &RyeOsCore) -> RyeOsAmbientVm {
    let Some(surface) = core
        .binding_attachment(&core.root_attachment_id)
        .and_then(|attachment| attachment.presentation.definition.surface().cloned())
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
/// The `@facet:` references in a view's declarations that the current
/// seat fold cannot resolve. Defaulting refs (`@facet:x|…`) never gate —
/// they resolve to their default. Generic over the grammar; the engine
/// never names a facet.
pub(crate) fn unsatisfied_facets(core: &RyeOsCore, binding: &ViewBinding) -> Vec<String> {
    let mut refs: Vec<String> = Vec::new();
    collect_required_facet_refs(&binding.body, &mut refs);
    for source in binding.sources.values() {
        collect_required_facet_refs(&source.params, &mut refs);
    }
    refs.sort();
    refs.dedup();
    let fold = core.seat.fold();
    let initial_route = core
        .active_view_set()
        .and_then(|view_set| core.insertion_attachment_id(view_set.id))
        .and_then(|attachment_id| core.binding_attachments.get(attachment_id))
        .map(|attachment| {
            serde_json::to_value(&attachment.initial_input_route).expect("InputRoute serializes")
        })
        .unwrap_or_else(|| serde_json::json!({}));
    refs.retain(|spec| {
        super::content::resolve_params(
            &serde_json::Value::String(format!("@facet:{spec}")),
            |key| {
                if key == super::seat::KEY_INPUT_ROUTE {
                    Some(initial_route.clone())
                } else if key == super::seat::KEY_SELECTION || key.starts_with("selection.") {
                    // Launcher availability describes a fresh mount in the
                    // active set, not an unrelated surface-global selection.
                    core.active_view_set()
                        .and_then(|set| core.selection_value_in_view_set(set.id, key))
                } else {
                    fold.get(key).cloned()
                }
            },
        )
        .is_null()
    });
    refs
}

fn collect_required_facet_refs(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => {
            if let Some(rest) = s.strip_prefix("@facet:")
                && !rest.contains('|')
            {
                out.push(rest.to_string());
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

fn dock_command_items(core: &RyeOsCore) -> Vec<RyeOsCommandChoice> {
    // Only surface-declared slots are toggleable; absent edges have no
    // slot and offer nothing. Labels stay mechanism words (edge names).
    [
        (
            RyeOsDockEdge::Bottom,
            "bottom",
            core.presentation
                .as_ref()
                .and_then(|presentation| presentation.docks().get("bottom")),
        ),
        (
            RyeOsDockEdge::Left,
            "left",
            core.presentation
                .as_ref()
                .and_then(|presentation| presentation.docks().get("left")),
        ),
        (
            RyeOsDockEdge::Right,
            "right",
            core.presentation
                .as_ref()
                .and_then(|presentation| presentation.docks().get("right")),
        ),
        (
            RyeOsDockEdge::Top,
            "top",
            core.presentation
                .as_ref()
                .and_then(|presentation| presentation.docks().get("top")),
        ),
    ]
    .into_iter()
    .filter_map(|(edge, name, slot)| slot.map(|slot| (edge, name, slot.visible)))
    .map(|(edge, name, visible)| RyeOsCommandChoice {
        label: format!("{} {name} slot", if visible { "Hide" } else { "Show" }),
        hint: "toggle edge slot".to_string(),
        intent: RyeOsUiIntent::ToggleDock { edge },
        secondary_intent: None,
        enabled: true,
    })
    .collect()
}

fn transfer_contact_command_items(core: &RyeOsCore) -> Vec<RyeOsCommandChoice> {
    use super::event::UiTransferContactIntent as Contact;
    use super::transfer_export::{UiTransferExport, UiTransferExportContent};
    let Some(presentation) = core.presentation.as_ref() else {
        return Vec::new();
    };
    let Some(session) = core.data.session.as_ref() else {
        return Vec::new();
    };
    let Some(producer) = core.presentation_operations.producer.as_ref() else {
        return Vec::new();
    };
    if core.presentation_operations.replay_blocked || !core.producer_matches_membership(producer) {
        return Vec::new();
    }
    let root = presentation.root_binding();
    let Some(descriptor) = core.binding_attachment(&root.binding_attachment_id) else {
        return Vec::new();
    };
    let max_bytes = descriptor.binding_request_bounds.max_request_bytes;
    let mut items = Vec::new();
    let mut push = |transfer: &ryeos_ui_presentation_contract::transfer::TransferIdentity,
                    action: Contact,
                    label: String| {
        items.push(RyeOsCommandChoice {
            label,
            hint: format!(
                "authenticated transfer {} · contact only",
                transfer.operation_id.chars().take(8).collect::<String>()
            ),
            intent: RyeOsUiIntent::ContactTransfer {
                transfer: transfer.clone(),
                action,
            },
            secondary_intent: None,
            enabled: true,
        });
    };
    for (frame, lifecycle) in core.authenticated_transfer_proofs.values() {
        let transfer = &frame.transfer;
        if lifecycle.transfer() != transfer
            || ![&transfer.source, &transfer.destination]
                .into_iter()
                .all(|seat| {
                    session.presentations.iter().any(|member| {
                        member.presentation_id == seat.presentation_id
                            && member.thread_id.as_deref() == Some(seat.thread_id.as_str())
                            && member.chain_root_id.as_deref() == Some(seat.chain_root_id.as_str())
                    })
                })
        {
            continue;
        }
        let source = presentation.id().as_str() == transfer.source.presentation_id
            && producer.thread_id == transfer.source.thread_id;
        let destination = presentation.id().as_str() == transfer.destination.presentation_id
            && producer.thread_id == transfer.destination.thread_id;
        if source {
            if lifecycle.prepared().is_some()
                && lifecycle.committed().is_none()
                && lifecycle.aborted().is_none()
            {
                push(transfer, Contact::Abort, "Abort transfer".into());
                if lifecycle.accepted().is_some() {
                    push(transfer, Contact::Commit, "Commit transfer".into());
                }
            }
        } else if destination {
            if lifecycle.accepted().is_some()
                && lifecycle.committed().is_some()
                && lifecycle.activated().is_none()
                && lifecycle.retained().is_none()
                && lifecycle.released().is_none()
            {
                push(
                    transfer,
                    Contact::Activate,
                    "Activate accepted transfer".into(),
                );
            }
            if lifecycle.prepared().is_none()
                || lifecycle.accepted().is_some()
                || lifecycle.aborted().is_some()
                || lifecycle.committed().is_some()
                || lifecycle.retained().is_some()
                || lifecycle.released().is_some()
            {
                continue;
            }
            let Some(prepared) = lifecycle.prepared() else {
                continue;
            };
            let Ok(export) = UiTransferExport::from_preparation(prepared.record, max_bytes) else {
                continue;
            };
            match export.content {
                UiTransferExportContent::ViewSets { .. } => {
                    for surface in presentation.surfaces().values() {
                        push(
                            transfer,
                            Contact::AcceptTabs {
                                surface_instance_id: surface.id().clone(),
                            },
                            format!("Accept tabs into {}", surface.title()),
                        );
                    }
                }
                UiTransferExportContent::SurfaceMount { .. } => {
                    for surface in presentation.surfaces().values() {
                        for set in surface.view_sets() {
                            let target = surface.id().clone();
                            if set.root.is_none() {
                                push(transfer, Contact::AcceptView {
                                    surface_instance_id: target, view_set_id: set.id,
                                    placement: super::composition_operation::CompositionPlacement::First,
                                }, format!("Accept view into empty {}", set.title));
                                continue;
                            }
                            fn groups(
                                root: &crate::layout::LayoutTree,
                                output: &mut Vec<crate::ids::ViewGroupId>,
                            ) {
                                match root {
                                    crate::layout::LayoutTree::Group { group_id, .. } => {
                                        output.push(*group_id)
                                    }
                                    crate::layout::LayoutTree::Split { first, second, .. } => {
                                        groups(first, output);
                                        groups(second, output);
                                    }
                                }
                            }
                            let mut group_ids = Vec::new();
                            groups(set.root.as_ref().unwrap(), &mut group_ids);
                            for group_id in group_ids {
                                push(transfer, Contact::AcceptView {
                                    surface_instance_id: target.clone(), view_set_id: set.id,
                                    placement: super::composition_operation::CompositionPlacement::GroupTab { group_id, before: None },
                                }, format!("Accept view as tab in {}", set.title));
                            }
                            if set.tiles.contains_key(&set.focused_tile) {
                                for axis in [
                                    crate::layout::SplitAxis::Horizontal,
                                    crate::layout::SplitAxis::Vertical,
                                ] {
                                    push(transfer, Contact::AcceptView {
                                        surface_instance_id: target.clone(), view_set_id: set.id,
                                        placement: super::composition_operation::CompositionPlacement::Split {
                                            target: set.focused_tile, axis, before: false, ratio: 0.5,
                                        },
                                    }, format!("Accept view beside focus in {}", set.title));
                                }
                            }
                        }
                    }
                }
                UiTransferExportContent::SharedDock { .. } => {
                    if let crate::presentation::UiCompositionDefinition::SharedPresentation {
                        config,
                    } = &descriptor.presentation.definition
                    {
                        for (edge, authored) in [
                            ("top", config.shared_presentation.docks.top.is_some()),
                            ("bottom", config.shared_presentation.docks.bottom.is_some()),
                            ("left", config.shared_presentation.docks.left.is_some()),
                            ("right", config.shared_presentation.docks.right.is_some()),
                        ] {
                            if authored && !presentation.docks().contains_key(edge) {
                                push(
                                    transfer,
                                    Contact::AcceptSharedDock { edge: edge.into() },
                                    format!("Accept dock at {edge}"),
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    if let Ok(replay) = core.acknowledged_composition_replay() {
        for freeze in replay.move_freezes.values() {
            let transfer = &freeze.transfer;
            if presentation.id().as_str() == transfer.source.presentation_id
                && producer.thread_id == transfer.source.thread_id
                && !core
                    .authenticated_cancellation_proofs
                    .contains_key(&transfer.operation_id)
            {
                push(transfer, Contact::CancelFreeze, "Cancel MOVE freeze".into());
            }
        }
    }
    items
}

fn transfer_begin_command_items(core: &RyeOsCore) -> Vec<RyeOsCommandChoice> {
    use super::event::UiTransferSourceSelection as Source;
    use super::transfer_export::UiTransferExportMode as Mode;
    let Some(presentation) = core.presentation.as_ref() else {
        return Vec::new();
    };
    let Some(session) = core.data.session.as_ref() else {
        return Vec::new();
    };
    let Some(producer) = core.presentation_operations.producer.as_ref() else {
        return Vec::new();
    };
    if !core.producer_matches_membership(producer) || !core.composition_write_available() {
        return Vec::new();
    }
    let Some(bounds) = core
        .binding_attachment(&presentation.root_binding().binding_attachment_id)
        .map(|binding| binding.binding_request_bounds)
    else {
        return Vec::new();
    };
    if bounds.max_request_bytes == 0 {
        return Vec::new();
    }
    let mut sources = Vec::new();
    for surface in presentation.ordered_surfaces() {
        if !surface.view_sets().is_empty() {
            sources.push((
                Source::SurfaceViewSets {
                    surface_instance_id: surface.id().clone(),
                },
                format!("all sets in {}", surface.title()),
            ));
        }
        for set in surface.view_sets() {
            sources.push((
                Source::ViewSet {
                    surface_instance_id: surface.id().clone(),
                    view_set_id: set.id,
                },
                format!("set {} in {}", set.title, surface.title()),
            ));
            if let Some(tile) = set.tiles.get(&set.focused_tile) {
                sources.push((
                    Source::MountedView {
                        instance_key: tile.instance_key.clone(),
                    },
                    format!("focused View in {}", set.title),
                ));
            }
        }
    }
    for (name, edge) in [
        ("top", RyeOsDockEdge::Top),
        ("bottom", RyeOsDockEdge::Bottom),
        ("left", RyeOsDockEdge::Left),
        ("right", RyeOsDockEdge::Right),
    ] {
        if presentation.docks().contains_key(name) {
            sources.push((Source::SharedDock { edge }, format!("{name} shared dock")));
        }
    }
    let destinations = session
        .presentations
        .iter()
        .filter(|member| {
            member.presentation_id != producer.presentation_id
                && member.thread_id.is_some()
                && member.chain_root_id.is_some()
                && session
                    .presentations
                    .iter()
                    .filter(|other| other.presentation_id == member.presentation_id)
                    .count()
                    == 1
        })
        .collect::<Vec<_>>();
    let mut items = Vec::new();
    for (source, source_label) in sources {
        for mode in [Mode::Move, Mode::Copy] {
            for destination in &destinations {
                let destination_label = destination
                    .surfaces
                    .first()
                    .map(|surface| surface.title.as_str())
                    .unwrap_or("other presentation");
                let mode_label = match mode {
                    Mode::Move => "Move",
                    Mode::Copy => "Copy",
                };
                let hint = match mode {
                    Mode::Move => "exact source capture; MOVE requires acknowledged freeze",
                    Mode::Copy => "fresh required inputs; no pins or drafts carried",
                };
                items.push(RyeOsCommandChoice {
                    label: format!(
                        "{mode_label} {source_label} to {destination_label} ({})",
                        destination.presentation_id
                    ),
                    hint: hint.into(),
                    intent: RyeOsUiIntent::BeginTransfer {
                        mode,
                        source: source.clone(),
                        destination_presentation_id: destination.presentation_id.clone(),
                    },
                    secondary_intent: None,
                    enabled: true,
                });
            }
        }
    }
    items
}

/// Focused table row affordances OTHER than its activate (Enter) intent.
/// Each is rebuilt as an `InvokeAffordance` from the row's raw record and
/// the view's declared affordances.
fn focused_row_command_items(
    core: &RyeOsCore,
    focused: Option<&RyeOsViewInstanceKey>,
) -> Vec<RyeOsCommandChoice> {
    let Some(instance_key) = focused.cloned() else {
        return Vec::new();
    };
    // Focus may be a shared dock or a captured mount, not the active tile.
    // Its affordances must come from that exact mounted View and binding.
    let Some(view_ref) = core.mounted_view_ref(&instance_key).map(str::to_owned) else {
        return Vec::new();
    };
    let Some(binding) = core.binding_for_instance(&instance_key, &view_ref) else {
        return Vec::new();
    };
    let Some(row) = focused_selected_table_row(core, focused) else {
        return Vec::new();
    };
    if row.raw.is_null() {
        return Vec::new();
    }
    let activate = binding
        .selection
        .as_ref()
        .and_then(|selection| selection.activate.as_deref());
    binding
        .affordances
        .iter()
        .filter_map(|aff| {
            let id = aff.get("id").and_then(|v| v.as_str())?;
            if Some(id) == activate {
                return None; // already the row's Enter intent
            }
            let eligibility = super::content::affordance_eligibility(aff, &row.raw);
            if !eligibility.visible {
                return None;
            }
            let label = aff
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or(id)
                .to_string();
            Some(RyeOsCommandChoice {
                label,
                hint: eligibility
                    .disabled_reason
                    .unwrap_or_else(|| "focused row".to_string()),
                intent: RyeOsUiIntent::InvokeAffordance {
                    instance_key: instance_key.clone(),
                    view_ref: view_ref.clone(),
                    affordance_id: id.to_string(),
                    record: row.raw.clone(),
                },
                secondary_intent: None,
                enabled: eligibility.enabled,
            })
        })
        .collect()
}

fn context_command_items(
    core: &RyeOsCore,
    focused: Option<&RyeOsViewInstanceKey>,
) -> Vec<RyeOsCommandChoice> {
    let mut items = Vec::new();
    items.push(RyeOsCommandChoice {
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
    let retry = focused.and_then(|instance| retry_intent_for_focused_row(core, instance));

    if let Some(intent) = inspect_intent_for_focused_row(core, focused) {
        let hint = if retry.is_some() {
            "Enter inspect · Shift+Enter retry".to_string()
        } else {
            focused_selection_hint(core, focused).unwrap_or_else(|| "focused row".to_string())
        };
        items.push(RyeOsCommandChoice {
            label: "Inspect selection".to_string(),
            hint,
            intent,
            secondary_intent: retry.clone(),
            enabled: true,
        });
    }

    if let Some(intent) = retry {
        items.push(RyeOsCommandChoice {
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
    items.extend(focused_row_command_items(core, focused));

    // Steering the active execution: offered only when the route has a head
    // thread. Each dispatches the shared SubmitThreadCommand → commands/submit.
    if let Some(head) = core.focused_input_route().thread {
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
            items.push(RyeOsCommandChoice {
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

#[cfg(test)]
pub(crate) fn available_command_items(core: &RyeOsCore) -> Vec<RyeOsCommandChoice> {
    let focused = core.focused_view_instance_key();
    available_command_items_with_focus(core, focused.as_ref())
}

/// Rebuild commands against a captured mounted origin without copying the seat log.
/// The ordinary command overlay continues to use live focus above.
pub(crate) fn available_command_items_with_focus(
    core: &RyeOsCore,
    focused: Option<&RyeOsViewInstanceKey>,
) -> Vec<RyeOsCommandChoice> {
    let mut items = context_command_items(core, focused);
    items.extend(dock_command_items(core));
    items.extend(transfer_begin_command_items(core));
    items.extend(transfer_contact_command_items(core));
    items.extend(core.releasable_binding_attachment_items());
    // Reuse the exact projected Surface actions. The command launcher is a
    // shared dialog View, including in the terminal; labels are navigation,
    // never a replacement for the projection's binding/close guard.
    for surface in session_vm(core).surfaces {
        if !surface.active {
            items.push(RyeOsCommandChoice {
                label: format!("Switch to {:02} {}", surface.ordinal, surface.title),
                hint: "select an open Surface".into(),
                intent: surface.select_intent,
                secondary_intent: None,
                enabled: core.composition_write_available()
                    && core
                        .presentation_operations
                        .producer
                        .as_ref()
                        .is_some_and(|producer| core.producer_matches_membership(producer)),
            });
        }
        if let Some(close_intent) = surface.close_intent {
            items.push(RyeOsCommandChoice {
                label: format!("Close surface {:02} {}", surface.ordinal, surface.title),
                hint: "close this exact Surface while preserving other open Surfaces".into(),
                intent: close_intent,
                secondary_intent: None,
                enabled: true,
            });
        }
    }
    // Presentation actions share the existing command overlay in both clients;
    // they are not executable Tool grants or session-local service routes.
    let can_open_document = core.supports_independent_documents
        && core
            .presentation_operations
            .producer
            .as_ref()
            .is_some_and(|producer| core.producer_matches_membership(producer))
        && !core.presentation_operations.replay_blocked
        && core
            .data
            .session
            .as_ref()
            .is_some_and(|session| !session.session_id.is_empty())
        && core.presentation.as_ref().is_some_and(|presentation| {
            let root = presentation.root_binding();
            core.binding_attachment(&root.binding_attachment_id)
                .is_some_and(|attachment| {
                    attachment.binding_generation == root.binding_generation
                        && attachment.binding_digest == root.binding_digest
                })
        });
    items.push(RyeOsCommandChoice {
        label: "New presentation document".into(),
        hint: if core.supports_independent_documents {
            "prepare configured opening tabs, then choose an explicit link; keep this presentation open"
        } else {
            "this host does not support independent presentation documents"
        }
        .into(),
        intent: RyeOsUiIntent::OpenNewPresentationDocument,
        secondary_intent: None,
        enabled: can_open_document,
    });
    items.push(RyeOsCommandChoice {
        label: "New view set".into(),
        hint: "open an empty arrangement".into(),
        intent: RyeOsUiIntent::NewViewSet,
        secondary_intent: None,
        enabled: core
            .active_surface_view_sets()
            .is_some_and(|sets| sets.len() < crate::surface::view_sets::MAX_VIEW_SETS),
    });
    let Some(view_set) = core.active_view_set() else {
        return items;
    };
    items.push(RyeOsCommandChoice {
        label: "Duplicate view set".into(),
        hint: "copy this composition with independent mounted views and drafts".into(),
        intent: RyeOsUiIntent::DuplicateViewSet {
            view_set_id: view_set.id,
        },
        secondary_intent: None,
        enabled: core
            .active_surface_view_sets()
            .is_some_and(|sets| sets.len() < crate::surface::view_sets::MAX_VIEW_SETS),
    });
    if let Some(instance) = focused {
        items.extend(
            selection_attachment_intents(core, instance)
                .into_iter()
                .map(|action| RyeOsCommandChoice {
                    label: action.label,
                    hint: action.title,
                    intent: action.intent,
                    secondary_intent: None,
                    enabled: true,
                }),
        );
    }
    if view_set.tiles.contains_key(&view_set.focused_tile) {
        items.extend(
            placement_intents(core, view_set.focused_tile)
                .into_iter()
                .map(|action| RyeOsCommandChoice {
                    label: action.label,
                    hint: action.title,
                    intent: action.intent,
                    secondary_intent: None,
                    enabled: true,
                }),
        );
        for destination in core.active_surface_view_sets().unwrap_or(&[]) {
            if destination.id == view_set.id {
                continue;
            }
            items.push(RyeOsCommandChoice {
                label: format!("Move view to {}", destination.title),
                hint: "move this mounted view and its drafts; preserve its subject attachment"
                    .into(),
                intent: RyeOsUiIntent::MoveTileToViewSet {
                    layout_guard: core.layout_guard(),
                    tile_id: view_set.focused_tile.0.to_string(),
                    view_set_id: destination.id,
                },
                secondary_intent: None,
                enabled: true,
            });
        }
        if let Some(presentation) = core.presentation.as_ref() {
            for surface in presentation.surfaces().values() {
                if core
                    .active_surface()
                    .is_some_and(|active| active.id() == surface.id())
                {
                    continue;
                }
                for destination in surface.view_sets() {
                    items.push(RyeOsCommandChoice {
                        label: format!("Move view to {} / {}", surface.title(), destination.title),
                        hint: "move within this presentation; preserve binding, subject and drafts"
                            .into(),
                        intent: RyeOsUiIntent::MoveTileToSurfaceViewSet {
                            layout_guard: core.layout_guard(),
                            tile_id: view_set.focused_tile.0.to_string(),
                            surface_instance_id: surface.id().clone(),
                            view_set_id: destination.id,
                        },
                        secondary_intent: None,
                        enabled: true,
                    });
                }
            }
        }
    }
    items.push(RyeOsCommandChoice {
        label: "Close view set".into(),
        hint: "close this arrangement, not its running work; current input must be cleared first"
            .into(),
        intent: RyeOsUiIntent::CloseViewSet {
            view_set_id: view_set.id,
        },
        secondary_intent: None,
        enabled: core
            .active_surface_view_sets()
            .is_some_and(|sets| sets.len() > 1)
            && core
                .input_buffers
                .iter()
                .filter(|(key, _)| {
                    view_set.tiles.values().any(|tile| {
                        super::model::InputBufferKey::storage_key_belongs_to(
                            key,
                            &tile.instance_key,
                        )
                    })
                })
                .all(|(_, input)| input.text.is_empty()),
    });
    items
}

fn inspect_intent_for_focused_row(
    core: &RyeOsCore,
    focused: Option<&RyeOsViewInstanceKey>,
) -> Option<RyeOsUiIntent> {
    match intent_for_focused_row_at(core, focused)? {
        intent @ (RyeOsUiIntent::InspectItem { .. }
        | RyeOsUiIntent::InspectThread { .. }
        | RyeOsUiIntent::InspectSummary { .. }
        | RyeOsUiIntent::ReadFile { .. }) => Some(intent),
        _ => None,
    }
}

fn focused_selection_hint(
    core: &RyeOsCore,
    focused: Option<&RyeOsViewInstanceKey>,
) -> Option<String> {
    if let Some(row) = focused_selected_row(core, focused) {
        return Some(row.secondary.or(row.meta).unwrap_or(row.primary));
    }
    // Table lens: the first cell is the row's identifier.
    focused_selected_table_row(core, focused).and_then(|row| row.cells.into_iter().next())
}

fn mounted_dialogs(core: &RyeOsCore) -> Vec<RyeOsDialogVm> {
    let Some(presentation) = core.presentation.as_ref() else {
        return Vec::new();
    };
    presentation
        .dialogs()
        .values()
        .filter_map(|mount| {
            if !mount.is_open() {
                return None;
            }
            let key = mount.view().instance_key();
            core.binding_attachment_for_instance(key)?;
            Some(RyeOsDialogVm {
                instance_key: key.clone(),
                title: mount.title().to_owned(),
                view: view_vm_for_instance(core, key)?,
                input: instance_input_vm(core, key, mount.view().view_ref()),
            })
        })
        .collect()
}

pub use super::keymap::shortcut_entries;
fn tile_intents(core: &RyeOsCore, tile_id: TileId) -> Vec<RyeOsTileIntentVm> {
    let Some(view_set) = core.active_view_set() else {
        return Vec::new();
    };
    let mut actions = placement_intents(core, tile_id);
    if let Some(tile) = view_set.tiles.get(&tile_id) {
        actions.extend(selection_attachment_intents(core, &tile.instance_key));
    }
    let tile_id = tile_id_text(tile_id);
    actions.push(RyeOsTileIntentVm {
        label: "Close view".to_string(),
        title: "Close this view; its running work is unaffected".to_string(),
        intent: RyeOsUiIntent::CloseTile { tile_id },
    });
    actions
}

fn selection_attachment_label(core: &RyeOsCore, instance: &RyeOsViewInstanceKey) -> Option<String> {
    // Placement never retargets an admitted view. Show its binding exception
    // independently of selection pin/follow, including selection-free views.
    let containing = core.view_set_for_instance(instance);
    let binding_label = core
        .binding_attachment_for_instance(instance)
        .and_then(|attachment| {
            (core.insertion_attachment_id(containing?.id)
                != Some(attachment.binding_attachment_id.as_str()))
            .then(|| {
                attachment
                    .project_path
                    .clone()
                    .unwrap_or_else(|| "Node scope".into())
            })
        });
    let participates = core
        .mounted_view_ref(instance)
        .and_then(|view_ref| core.binding_for_instance(instance, view_ref))
        .is_some_and(super::attachment::participates_in_selection);
    let selection_label = match core
        .selection_attachment_for_instance(instance)
        .filter(|_| participates)
    {
        Some(super::attachment::SelectionAttachment::Pinned { .. }) => {
            Some("Selection pinned".to_string())
        }
        Some(super::attachment::SelectionAttachment::RequiredSubject { input, .. }) => {
            Some(format!("Subject required · {input}"))
        }
        Some(super::attachment::SelectionAttachment::FollowViewSet { view_set_id }) => {
            (containing?.id != view_set_id).then(|| {
                core.view_sets()
                    .find(|set| set.id == view_set_id)
                    .map(|set| format!("Following {}", set.title))
                    .unwrap_or_else(|| "Selection owner unavailable".into())
            })
        }
        None => None,
    };
    match (binding_label, selection_label) {
        (Some(binding), Some(selection)) => Some(format!("{binding} · {selection}")),
        (Some(label), None) | (None, Some(label)) => Some(label),
        (None, None) => None,
    }
}

fn selection_attachment_intents(
    core: &RyeOsCore,
    instance: &RyeOsViewInstanceKey,
) -> Vec<RyeOsTileIntentVm> {
    let Some(binding) = core
        .mounted_view_ref(instance)
        .and_then(|view_ref| core.binding_for_instance(instance, view_ref))
    else {
        return Vec::new();
    };
    if !super::attachment::participates_in_selection(binding) {
        return Vec::new();
    }
    let can_pin = !super::attachment::selection_dependencies(binding).is_empty();
    let current = core.selection_attachment_for_instance(instance);
    if let Some(super::attachment::SelectionAttachment::RequiredSubject { facets, .. }) = &current {
        return required_subject_actions(core, instance, facets);
    }
    let mut actions = Vec::new();
    if can_pin
        && !matches!(
            current,
            Some(super::attachment::SelectionAttachment::Pinned { .. })
        )
    {
        actions.push(RyeOsTileIntentVm {
            label: "Pin this selection".into(),
            title: "Keep this view on the captured selection; later selections do not retarget it"
                .into(),
            intent: RyeOsUiIntent::PinViewSelection {
                instance_key: instance.clone(),
            },
        });
    }
    if can_pin
        && core.active_view_set().map(|set| set.tiling.mode)
            != Some(crate::surface::TilingModeSpec::SingleLens)
    {
        actions.push(RyeOsTileIntentVm {
            label: "Open pinned alongside".into(),
            title: "Open another instance fixed to this view's exact current selection".into(),
            intent: RyeOsUiIntent::OpenPinnedViewAlongside {
                instance_key: instance.clone(),
            },
        });
    }
    for set in core.active_surface_view_sets().unwrap_or(&[]) {
        if matches!(&current, Some(super::attachment::SelectionAttachment::FollowViewSet { view_set_id }) if *view_set_id == set.id)
        {
            continue;
        }
        actions.push(RyeOsTileIntentVm {
            label: format!("Follow selection in {}", set.title),
            title: "Explicitly reattach this view's selection; its composer route is unchanged"
                .into(),
            intent: RyeOsUiIntent::FollowViewSetSelection {
                instance_key: instance.clone(),
                view_set_id: set.id,
            },
        });
    }
    actions
}

fn required_subject_actions(
    core: &RyeOsCore,
    instance: &RyeOsViewInstanceKey,
    facets: &[String],
) -> Vec<RyeOsTileIntentVm> {
    let mut title_counts = std::collections::BTreeMap::new();
    for set in core.active_surface_view_sets().unwrap_or(&[]) {
        *title_counts.entry(set.title.as_str()).or_insert(0usize) += 1;
    }
    core.active_surface_view_sets()
        .unwrap_or(&[])
        .iter()
        .enumerate()
        .filter(|(_, set)| core.subject_values_from_view_set(set.id, facets).is_some())
        .flat_map(|(index, set)| {
            let label = if title_counts.get(set.title.as_str()).copied().unwrap_or(0) > 1 {
                format!("Use selection in {} · set {}", set.title, index + 1)
            } else {
                format!("Use selection in {}", set.title)
            };
            let follow_label = if title_counts.get(set.title.as_str()).copied().unwrap_or(0) > 1 {
                format!("Follow selection in {} · set {}", set.title, index + 1)
            } else {
                format!("Follow selection in {}", set.title)
            };
            [
                RyeOsTileIntentVm {
                    title: format!(
                        "Pin the exact current selection to {} without changing its binding",
                        instance.as_str()
                    ),
                    label,
                    intent: RyeOsUiIntent::SupplyRequiredSubject {
                        instance_key: instance.clone(),
                        source_view_set_id: set.id,
                    },
                },
                RyeOsTileIntentVm {
                    title: "Follow this set as its compatible selection changes".into(),
                    label: follow_label,
                    intent: RyeOsUiIntent::FollowViewSetSelection {
                        instance_key: instance.clone(),
                        view_set_id: set.id,
                    },
                },
            ]
        })
        .collect()
}

/// Both renderers receive exact mounted coordinates from this projection.
/// Do not reconstruct placement authority from browser labels or DOM order.
fn placement_intents(core: &RyeOsCore, tile_id: TileId) -> Vec<RyeOsTileIntentVm> {
    let Some(view_set) = core.active_view_set() else {
        return Vec::new();
    };
    use crate::view_set::FocusDirection;

    if !view_set.tiles.contains_key(&tile_id) {
        return Vec::new();
    }
    let guard = core.layout_guard();
    let mut actions = Vec::new();
    if view_set.tiling.mode == crate::surface::TilingModeSpec::MasterStack
        && view_set.tile_ids().len() > 1
        && view_set.tile_ids().first().copied() != Some(tile_id)
    {
        actions.push(RyeOsTileIntentVm {
            label: "Promote to master".into(),
            title: "Promote this exact mounted view without changing its subject".into(),
            intent: RyeOsUiIntent::PromoteTileToMaster {
                layout_guard: guard.clone(),
                tile_id: tile_id_text(tile_id),
            },
        });
    }
    if let Some(tabs) = view_set
        .root
        .as_ref()
        .and_then(|root| root.group_tabs(tile_id))
        && let Some(index) = tabs.iter().position(|candidate| *candidate == tile_id)
    {
        if index > 0 {
            actions.push(RyeOsTileIntentVm {
                label: "Move tab left".into(),
                title: "Move this view one place earlier in its current group".into(),
                intent: RyeOsUiIntent::MoveTileToGroup {
                    layout_guard: guard.clone(),
                    tile_id: tile_id_text(tile_id),
                    target_tile_id: tile_id_text(tabs[index - 1]),
                    index: index - 1,
                },
            });
        }
        if index + 1 < tabs.len() {
            actions.push(RyeOsTileIntentVm {
                label: "Move tab right".into(),
                title: "Move this view one place later in its current group".into(),
                intent: RyeOsUiIntent::MoveTileToGroup {
                    layout_guard: guard.clone(),
                    tile_id: tile_id_text(tile_id),
                    target_tile_id: tile_id_text(tabs[index + 1]),
                    // The source is removed before insertion, so the position
                    // after its next sibling is the original index + 1.
                    index: index + 1,
                },
            });
        }
    }
    for target_id in view_set.tile_ids() {
        if target_id == tile_id {
            continue;
        }
        let Some(target) = view_set.tiles.get(&target_id) else {
            continue;
        };
        let target_label = format!("{} · {}", tile_title(core, target), target_id.0);
        for (label, edge) in [
            ("Left of", FocusDirection::Left),
            ("Right of", FocusDirection::Right),
            ("Above", FocusDirection::Up),
            ("Below", FocusDirection::Down),
        ] {
            actions.push(RyeOsTileIntentVm {
                label: format!("{label} {target_label}"),
                title: "Move this mounted view, preserving its subject and draft".into(),
                intent: RyeOsUiIntent::MoveTileBeside {
                    layout_guard: guard.clone(),
                    tile_id: tile_id_text(tile_id),
                    target_tile_id: tile_id_text(target_id),
                    edge,
                },
            });
        }
        actions.push(RyeOsTileIntentVm {
            label: format!("Group with {target_label}"),
            title: "Move this view into the destination tab group".into(),
            intent: RyeOsUiIntent::MoveTileToGroup {
                layout_guard: guard.clone(),
                tile_id: tile_id_text(tile_id),
                target_tile_id: tile_id_text(target_id),
                index: 0,
            },
        });
    }
    actions
}

pub(crate) fn intent_for_focused_row(core: &RyeOsCore) -> Option<RyeOsUiIntent> {
    let focused = core.focused_view_instance_key();
    intent_for_focused_row_at(core, focused.as_ref())
}

fn intent_for_focused_row_at(
    core: &RyeOsCore,
    focused: Option<&RyeOsViewInstanceKey>,
) -> Option<RyeOsUiIntent> {
    if let Some(instance_key) = focused.cloned()
        && let Some((_view_ref, field)) = field_vm_for_instance(core, &instance_key)
        && let Some(intent) = field.selected_activation_intent()
    {
        return Some(intent);
    }
    // Feed lens: activation acts on the entry under the point (e.g. enter a
    // forked subthread, inspect an error terminal), not a row.
    if let Some(entry) = focused.and_then(|instance| focused_timeline_entry(core, instance)) {
        return match entry {
            RyeOsTimelineEntryVm::Line { intent, .. } => intent,
            _ => None,
        };
    }
    if let Some(intent) = focused_selected_row(core, focused).and_then(|row| row.intent) {
        return Some(intent);
    }
    // Table lens: rows carry the same activation affordance, on a distinct VM.
    focused_selected_table_row(core, focused).and_then(|row| row.intent)
}

/// The row under the point in the exact focused mount, if the point is on a row. The
/// rows widget indexes by the flat cursor; sections carry the selection on the
/// row VM itself (the point may instead be on a collapsed header → no row).
/// Scene widgets (graph/atlas) have no rows.
fn focused_selected_row(
    core: &RyeOsCore,
    focused: Option<&RyeOsViewInstanceKey>,
) -> Option<RyeOsRowVm> {
    let instance_key = focused?;
    match view_vm_for_instance(core, instance_key)? {
        RyeOsViewVm::Rows { rows, .. } => rows.into_iter().find(|row| row.selected),
        RyeOsViewVm::Sections { sections, .. } => sections
            .into_iter()
            .flat_map(|section| section.rows)
            .find(|row| row.selected),
        _ => None,
    }
}

/// The table row under the point in the exact focused mount. Table rows are a distinct
/// VM (`RyeOsTableRowVm`, columnar cells) from the rows widget, so they need
/// their own selection projection — same flat cursor, different shape.
fn focused_selected_table_row(
    core: &RyeOsCore,
    focused: Option<&RyeOsViewInstanceKey>,
) -> Option<RyeOsTableRowVm> {
    let instance_key = focused?;
    match view_vm_for_instance(core, instance_key)? {
        RyeOsViewVm::Table { rows, .. } => rows.into_iter().find(|row| row.selected),
        _ => None,
    }
}

fn selected_cursor(core: &RyeOsCore, tile_id: TileId) -> Option<usize> {
    let view_set = core.active_view_set()?;
    let tile = view_set.tiles.get(&tile_id)?;
    match &tile.local {
        ViewLocalState::GenericList { cursor, .. } => Some(*cursor),
        ViewLocalState::None | ViewLocalState::Field(_) => None,
    }
}

fn selected_collapsed(
    core: &RyeOsCore,
    tile_id: TileId,
) -> Option<&std::collections::BTreeSet<usize>> {
    let view_set = core.active_view_set()?;
    match &view_set.tiles.get(&tile_id)?.local {
        ViewLocalState::GenericList { collapsed, .. } => Some(collapsed),
        ViewLocalState::None | ViewLocalState::Field(_) => None,
    }
}

type SelectedRowState<'a> = (
    Option<&'a std::collections::BTreeSet<String>>,
    Option<&'a std::collections::BTreeSet<String>>,
    Option<&'a std::collections::BTreeMap<String, crate::view_set::RowFlash>>,
);

fn selected_row_state(core: &RyeOsCore, tile_id: TileId) -> SelectedRowState<'_> {
    let Some(view_set) = core.active_view_set() else {
        return (None, None, None);
    };
    match view_set.tiles.get(&tile_id).map(|tile| &tile.local) {
        Some(ViewLocalState::GenericList {
            expanded_rows,
            collapsed_tree_rows,
            changed_rows,
            ..
        }) => (
            Some(expanded_rows),
            Some(collapsed_tree_rows),
            Some(changed_rows),
        ),
        _ => (None, None, None),
    }
}

fn health_label(core: &RyeOsCore) -> String {
    // Authenticated transport is not a node-wide operational health claim.
    // Neutral presentations require no dimension observation to connect.
    let freshness = core.runtime.transport.overall_freshness();
    let transport = match freshness {
        RyeOsTransportFreshness::Connecting => "connecting",
        RyeOsTransportFreshness::Current => "connected",
        RyeOsTransportFreshness::Reconnecting => "reconnecting",
        RyeOsTransportFreshness::GapResnapshotRequired => "resynchronization required",
        RyeOsTransportFreshness::ExpiredOrRevoked => "session expired or revoked",
    };
    transport.to_owned()
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
                .unwrap_or("unqualified")
                .to_string()
        })
}

fn normalize_version_label(version: &str) -> String {
    version.trim().trim_start_matches("ryeosd-").to_string()
}

fn tone_for_health(value: &str) -> RyeOsTone {
    match value {
        "connected" => RyeOsTone::Good,
        "reconnecting" | "resynchronization required" => RyeOsTone::Warn,
        "session expired or revoked" => RyeOsTone::Danger,
        _ => RyeOsTone::Neutral,
    }
}

fn subtitle(core: &RyeOsCore) -> String {
    let Some(view_set) = core.active_view_set() else {
        return String::new();
    };
    core.insertion_attachment_id(view_set.id)
        .and_then(|id| core.binding_attachment(id))
        .map(|attachment| {
            attachment
                .project_path
                .clone()
                .unwrap_or_else(|| "Node scope".into())
        })
        .unwrap_or_else(|| "Binding unavailable".into())
}

fn tile_id_text(id: TileId) -> String {
    id.0.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::content::{ProjectedRecord, TimelineRole};
    use crate::ui::reducer::test_support::core as test_support_core;
    use crate::ui::reducer::test_support::session_with_surface_and_docks;
    use crate::ui::reducer::test_support::*;
    use crate::ui::{RyeOsEvent, RyeOsUiEvent};
    use serde_json::json;

    #[test]
    fn authored_table_coverage_never_infers_completeness_from_rows() {
        let binding: ViewBinding = serde_json::from_value(json!({
            "widget": "table",
            "sources": {"default": {
                "ref": "service:test/list", "collection": "threads", "coverage": "coverage"
            }}
        }))
        .unwrap();
        let observed = table_source_coverage(
            &binding,
            &json!({"threads":[{"thread_id":"T-one"}], "coverage":{
                "state":"observed", "reason":"projection_locator_not_exhaustive"
            }}),
        )
        .unwrap();
        assert_eq!(observed.state, "observed");
        assert_eq!(observed.reason, "projection_locator_not_exhaustive");
        let missing = table_source_coverage(&binding, &json!({"threads":[]})).unwrap();
        assert_eq!(missing.state, "unavailable");
        assert_eq!(missing.reason, "coverage_not_reported");
        for state in ["partial", "failed", "unavailable"] {
            let report = table_source_coverage(&binding, &json!({
                "threads": [], "coverage": {"state": state, "reason": "bounded_live_enumeration"}
            })).unwrap();
            assert_eq!(report.state, state);
        }
        let unavailable = table_source_coverage(
            &binding,
            &json!({
                "threads": [], "coverage": {"state": "unavailable", "reason": "kind_not_registered"}
            }),
        )
        .unwrap();
        assert_eq!(unavailable.state, "unavailable");
        assert_eq!(unavailable.reason, "kind_not_registered");
    }

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

    fn tile_default_source_key(core: &RyeOsCore, tile_id: TileId) -> String {
        super::super::source_key::RyeOsSourceInstanceKey::named(
            core.active_view_set().unwrap().tiles[&tile_id]
                .instance_key
                .clone(),
            "default",
        )
        .encode()
    }

    fn dock_default_source_key(core: &RyeOsCore, edge: RyeOsDockEdge) -> String {
        super::super::source_key::RyeOsSourceInstanceKey::named(
            core.shared_dock_instance_key(edge).unwrap(),
            "default",
        )
        .encode()
    }

    #[test]
    fn failed_source_renders_error_instead_of_loading_forever() {
        let view_ref = "view:test/threads";
        let mut core = session_with_views(
            json!({view_ref:{"widget":"rows","sources":{"default":{
            "ref":"service:ui/ryeos-ui/threads/list","collection":"threads"}}}}),
            json!([view_ref]),
        );
        let tile = core.active_view_set().unwrap().focused_tile;
        let instance_key = core.active_view_set().unwrap().tiles[&tile]
            .instance_key
            .clone();
        let source_key = super::super::source_key::RyeOsSourceInstanceKey::named(
            instance_key.clone(),
            "default",
        )
        .encode();
        core.data
            .source_errors
            .insert(source_key, "metadata type mismatch".into());

        let view = bound_view_vm_keyed(
            &core,
            &instance_key,
            None,
            RowLocalState {
                cursor: None,
                collapsed: None,
                expanded_rows: None,
                collapsed_tree_rows: None,
                changed_rows: None,
            },
            view_ref,
            &core.ui.atlas,
        );
        match view {
            RyeOsViewVm::Placeholder { message, .. } => {
                assert!(message.contains("failed"));
                assert!(message.contains("metadata type mismatch"));
                assert!(!message.contains("loading"));
            }
            other => panic!("expected source error placeholder, got {other:?}"),
        }
    }

    fn source_status_fixture(widget: Value) -> (RyeOsCore, TileId) {
        let core = session_with_views(
            json!({"view:test/source-status":widget}),
            json!(["view:test/source-status"]),
        );
        let tile = core.active_view_set().unwrap().focused_tile;
        (core, tile)
    }

    #[test]
    fn sections_report_independent_failure_and_unresolved_instead_of_empty_success() {
        let (mut core, tile) = source_status_fixture(json!({
            "widget": "sections",
            "sources": {"good": {"ref":"service:good"}, "denied": {"ref":"service:denied"}, "unresolved": {"ref":"service:unresolved"}},
            "sections": [
                {"title":"Good", "source_channel":"good", "collection":"rows", "projection":{"primary":"name"}},
                {"title":"Denied", "source_channel":"denied", "collection":"rows"},
                {"title":"Unresolved", "source_channel":"unresolved", "collection":"rows"}
            ]
        }));
        let instance = core.active_view_set().unwrap().tiles[&tile]
            .instance_key
            .clone();
        let key = |channel: &str| {
            super::super::source_key::RyeOsSourceInstanceKey::named(instance.clone(), channel)
                .encode()
        };
        core.data
            .sources
            .insert(key("good"), json!({"rows":[{"name":"visible"}]}));
        core.data.source_errors.insert(
            key("denied"),
            super::super::effect::RyeOsUiError::definite("service_cap_denied", "Access denied"),
        );
        core.data.source_unresolved.insert(key("unresolved"), true);
        let RyeOsViewVm::Sections { sections, .. } =
            bound_view_vm(&core, tile, "view:test/source-status")
        else {
            panic!("sections")
        };
        assert_eq!(sections[0].rows[0].primary, "visible");
        assert!(sections[0].source_status.is_none());
        let denied = sections[1].source_status.as_ref().unwrap();
        assert_eq!(denied.state, "failed");
        assert!(denied.message.contains("service_cap_denied"));
        assert!(!denied.stale);
        assert!(sections[1].rows.is_empty());
        assert_eq!(
            sections[2].source_status.as_ref().unwrap().state,
            "unresolved"
        );
    }

    #[test]
    fn timeline_loading_uses_its_source_not_an_unrelated_conversation_route() {
        let (mut core, tile) = source_status_fixture(json!({
            "widget": "timeline",
            "sources": {"default":{"ref":"service:events/node", "collection":"events"}}
        }));
        assert!(core.focused_input_route().chain_root.is_none());
        let RyeOsViewVm::Placeholder { message, .. } =
            bound_view_vm(&core, tile, "view:test/source-status")
        else {
            panic!("waiting timeline")
        };
        assert!(message.contains("loading service:events/node"));
        assert!(!message.contains("conversation"));
        let key = tile_default_source_key(&core, tile);
        core.data
            .source_errors
            .insert(key, "Denied event source".into());
        let RyeOsViewVm::Placeholder { message, .. } =
            bound_view_vm(&core, tile, "view:test/source-status")
        else {
            panic!("failed timeline")
        };
        assert!(message.contains("failed"));
        assert!(message.contains("Denied event source"));
    }

    #[test]
    fn refresh_failure_keeps_rows_only_with_explicit_stale_status() {
        let (mut core, tile) = source_status_fixture(json!({
            "widget": "table", "sources": {"default":{"ref":"service:list", "collection":"rows"}},
            "projections": {"columns":[{"label":"Name", "field":"name"}]}
        }));
        let key = tile_default_source_key(&core, tile);
        core.data
            .sources
            .insert(key.clone(), json!({"rows":[{"name":"last observed"}]}));
        core.data.source_stored_epoch.insert(key.clone(), 10);
        core.data.source_epoch.insert(key.clone(), 11);
        assert_eq!(source_status(&core, &key).unwrap().state, "refreshing");
        core.data
            .source_errors
            .insert(key.clone(), "Refresh refused".into());
        let RyeOsViewVm::Table {
            rows,
            source_status: Some(status),
            ..
        } = bound_view_vm(&core, tile, "view:test/source-status")
        else {
            panic!("stale table")
        };
        assert_eq!(rows[0].cells[0], "last observed");
        assert_eq!(status.state, "failed");
        assert!(status.stale);
        core.data.source_errors.remove(&key);
        core.data.source_stored_epoch.insert(key.clone(), 11);
        assert!(source_status(&core, &key).is_none());
    }

    #[test]
    fn unresolved_subject_projects_structured_shared_completion_actions() {
        let view_ref = "view:test/subject";
        let mut core = session_with_views(
            json!({ (view_ref): {
                "widget": "rows",
                "sources": {"default": {
                    "ref": "service:test/subject",
                    "params": {"thread": "@facet:selection.work.thread"}
                }}
            }}),
            json!([view_ref]),
        );
        let tile_id = core.active_view_set().unwrap().focused_tile;
        let instance = core.active_view_set().unwrap().tiles[&tile_id]
            .instance_key
            .clone();
        let source_set = core.active_view_set().unwrap().id;
        let selection_key =
            super::super::seat::selection_storage_key(source_set, "selection.work").unwrap();
        core.seat
            .append_facet(selection_key, json!({"thread": "T-current"}));
        core.selection_attachments.insert(
            instance.clone(),
            super::super::attachment::SelectionAttachment::RequiredSubject {
                input: "subject_tile_0".into(),
                facets: vec!["selection.work.thread".into()],
            },
        );

        let view = bound_view_vm_keyed(
            &core,
            &instance,
            None,
            RowLocalState {
                cursor: None,
                collapsed: None,
                expanded_rows: None,
                collapsed_tree_rows: None,
                changed_rows: None,
            },
            view_ref,
            &core.ui.atlas,
        );
        let RyeOsViewVm::RequiredSubject {
            input,
            facets,
            actions,
            ..
        } = view
        else {
            panic!("unresolved subject must use the structured shared VM")
        };
        assert_eq!(input, "subject_tile_0");
        assert_eq!(facets, vec!["selection.work.thread"]);
        assert!(actions.iter().any(|action| matches!(
            &action.intent,
            RyeOsUiIntent::SupplyRequiredSubject { source_view_set_id, .. }
                if *source_view_set_id == source_set
        )));
        assert!(actions.iter().any(|action| matches!(
            &action.intent,
            RyeOsUiIntent::FollowViewSetSelection { view_set_id, .. }
                if *view_set_id == source_set
        )));
        let commands = available_command_items_with_focus(&core, Some(&instance));
        assert!(commands.iter().any(|command| matches!(
            &command.intent,
            RyeOsUiIntent::SupplyRequiredSubject { instance_key, source_view_set_id }
                if instance_key == &instance && *source_view_set_id == source_set
        )));
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
        let session = session_with_surface(json!({
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
        }));
        let core = test_support_core(session);
        let vm = build_view_model(&core);

        assert!(vm.view_set.as_ref().unwrap().center_is_empty);
        let backdrop = vm.backdrop.expect("backdrop scene on empty center");
        // The scene resolves from the view body — objects incl. text labels.
        assert!(!backdrop.objects.is_empty());
        assert!(
            backdrop
                .objects
                .iter()
                .any(|o| o.label.as_deref() == Some("RYE OS"))
        );
    }

    #[test]
    fn status_view_in_top_dock_includes_notice_rows() {
        let session = session_with_surface_and_docks(
            json!({
                "name": "ryeos-base",

                "views": {
                    "view:ryeos/node/status": {
                        "title": "Status",
                        "widget": "key_value",
                        "sources": { "default": { "ref": "service:node/status" } },
                        "projections": { "detail": ["version"] }
                    }
                }
            }),
            serde_json::json!({
                "top": { "content": "view:ryeos/node/status", "open": true, "size": 3 }
            }),
        );
        let mut core = test_support_core(session);
        core.data.sources.insert(
            dock_default_source_key(&core, RyeOsDockEdge::Top),
            json!({ "version": "0.1.0" }),
        );
        core.notice(
            "Queued behind active thread · 2 staged inputs.",
            RyeOsTone::Accent,
        );

        let vm = build_view_model(&core);
        let dock = vm
            .docks
            .top
            .expect("authored top status slot should render");
        assert_eq!(
            dock.instance_key,
            core.shared_dock_instance_key(RyeOsDockEdge::Top).unwrap()
        );
        assert_eq!(dock.edge, RyeOsDockEdge::Top);
        assert_eq!(dock.title, "Status");
        assert_eq!(dock.size, 3);
        match dock.view {
            RyeOsViewVm::Rows { rows, .. } => {
                assert!(rows.iter().any(|row| row.primary == "version: 0.1.0"));
                let version = rows
                    .iter()
                    .find(|row| row.primary == "version: 0.1.0")
                    .unwrap();
                assert_eq!(version.kind.as_deref(), Some("detail"));
                assert_eq!(version.detail[0].field, "version");
                assert_eq!(version.detail[0].value, "0.1.0");
                assert!(version.intent.is_none());
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
        let session = session_with_surface(json!({ "name": "ryeos-base", "views": {} }));
        let core = test_support_core(session);
        let vm = build_view_model(&core);
        assert!(vm.view_set.as_ref().unwrap().center_is_empty);
        assert!(vm.backdrop.is_none());
    }

    fn session_with_views(views: serde_json::Value, tiles: serde_json::Value) -> RyeOsCore {
        let session = session_with_surface(json!({
            "name": "ryeos-base",
            "tiles": tiles,
            "views": views,
        }));
        test_support_core(session)
    }

    #[test]
    fn projected_placement_action_cannot_cross_view_set_switch() {
        let mut core = session_with_views(
            json!({
                "view:test/one": { "widget": "rows" },
                "view:test/two": { "widget": "rows" }
            }),
            json!(["view:test/one", "view:test/two"]),
        );
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let source = core.active_view_set().unwrap().tile_ids()[0];
        let action = placement_intents(&core, source)
            .into_iter()
            .find(|action| matches!(&action.intent, RyeOsUiIntent::MoveTileBeside { .. }))
            .expect("placement action");
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::NewViewSet,
            },
        });
        settle_pending_prefix(&mut core, &producer);
        assert_eq!(core.active_surface().unwrap().view_sets().len(), 2);
        let before = core.export_layout_preferences().expect("layout snapshot");
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: action.intent,
            },
        });
        assert_eq!(core.export_layout_preferences().unwrap(), before);
    }

    #[test]
    fn master_promotion_is_projected_for_and_applied_to_the_exact_tile() {
        let mut core = session_with_views(
            json!({
                "view:test/one": { "widget": "rows" },
                "view:test/two": { "widget": "rows" }
            }),
            json!(["view:test/one", "view:test/two"]),
        );
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let tiles = core.active_view_set().unwrap().tile_ids();
        assert!(
            placement_intents(&core, tiles[0])
                .into_iter()
                .all(|action| !matches!(action.intent, RyeOsUiIntent::PromoteTileToMaster { .. })),
            "the current master must not advertise an action that demotes it"
        );
        let promoted = tiles[1];
        let action = placement_intents(&core, promoted)
            .into_iter()
            .find(|action| matches!(&action.intent, RyeOsUiIntent::PromoteTileToMaster { .. }))
            .expect("master promotion action");
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: action.intent,
            },
        });
        assert_eq!(core.active_view_set().unwrap().tile_ids()[0], tiles[0]);
        settle_pending_prefix(&mut core, &producer);
        assert_eq!(core.active_view_set().unwrap().tile_ids()[0], promoted);
    }

    #[test]
    fn grouped_tab_reorder_is_an_exact_projected_placement_action() {
        let mut core = test_support_core(session_with_surface(json!({
            "name": "Grouped",
            "views": {
                "view:test/one": { "widget": "rows" },
                "view:test/two": { "widget": "rows" }
            },
            "view_sets": [{"schema":2,"id":"work","title":"Work","root":{
                "type":"group","views":["view:test/one","view:test/two"],"active":0
            }}]
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let tiles = core.active_view_set().unwrap().tile_ids();
        let action = placement_intents(&core, tiles[1])
            .into_iter()
            .find(|action| action.label == "Move tab left")
            .expect("tab reorder action");
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: action.intent,
            },
        });
        settle_pending_prefix(&mut core, &producer);
        let tabs = core
            .active_view_set()
            .unwrap()
            .root
            .as_ref()
            .and_then(|root| root.group_tabs(tiles[1]))
            .expect("grouped tabs");
        assert_eq!(tabs, [tiles[1], tiles[0]]);
    }

    #[test]
    fn neutral_home_has_no_implicit_domain_observations_or_input_route() {
        let surfaces = ryeos_engine::test_support::workspace_root()
            .join("bundles/ryeos-ui/.ai/surfaces/ryeos/ui");
        let home: serde_json::Value =
            serde_yaml::from_str(&std::fs::read_to_string(surfaces.join("base.yaml")).unwrap())
                .unwrap();
        assert_eq!(home["sources"], json!({}));
        assert_eq!(home["capabilities"]["sources"], json!([]));
        assert_eq!(home["capabilities"]["affordances"], json!([]));
        for key in ["tiles", "library", "affordances"] {
            assert_eq!(home[key], json!([]), "neutral {key}");
        }
        assert!(home.get("input").is_none());
        assert_eq!(home["backdrop"], "view:ryeos/backdrop/prism-shards");
        let observe: serde_json::Value = serde_yaml::from_str(
            &std::fs::read_to_string(surfaces.join("base-observe.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(observe["extends"], "surface:ryeos/ui/base");
        assert_eq!(observe["capabilities"]["sources"], json!([]));
    }

    #[test]
    fn neutral_chrome_reports_transport_without_claiming_node_health() {
        let mut core = RyeOsCore::default();
        assert!(core.data.dimension.is_none());
        assert_eq!(health_label(&core), "connected");
        assert_eq!(tone_for_health(&health_label(&core)), RyeOsTone::Good);
        for (freshness, expected) in [
            (RyeOsTransportFreshness::Connecting, "connecting"),
            (RyeOsTransportFreshness::Reconnecting, "reconnecting"),
            (
                RyeOsTransportFreshness::GapResnapshotRequired,
                "resynchronization required",
            ),
            (
                RyeOsTransportFreshness::ExpiredOrRevoked,
                "session expired or revoked",
            ),
        ] {
            core.runtime
                .transport
                .channels
                .get_mut(&RyeOsTransportChannel::Session)
                .unwrap()
                .freshness = freshness;
            assert_eq!(health_label(&core), expected);
        }
    }

    #[test]
    fn quiet_status_line_does_not_repeat_root_context_or_execution_counters() {
        let core = RyeOsCore::default();
        let healthy = RyeOsChromeVm {
            title: "RyeOS".into(),
            subtitle: String::new(),
            health_label: "healthy".into(),
            health_tone: RyeOsTone::Good,
        };
        assert!(status_bar_vm(&healthy, &core).segments.is_empty());

        let interrupted = RyeOsChromeVm {
            health_label: "connection interrupted".into(),
            health_tone: RyeOsTone::Warn,
            ..healthy
        };
        let status = status_bar_vm(&interrupted, &core);
        assert!(status.segments.is_empty());
    }

    #[test]
    fn document_widget_projects_bounded_source_fields() {
        let view_ref = "view:test/document";
        let mut core = session_with_views(
            json!({
                (view_ref): {
                    "widget": "document",
                    "sources": { "default": { "ref": "service:test/read" } },
                    "projections": {
                        "path": "result.path",
                        "content": "result.body",
                        "truncated": "result.bounded"
                    }
                }
            }),
            json!([view_ref]),
        );
        let tile_id = core.active_surface_view_sets().unwrap()[0].tile_ids()[0];
        core.data.sources.insert(
            tile_default_source_key(&core, tile_id),
            json!({ "result": { "path": "src/main.rs", "body": "fn main() {}", "bounded": true } }),
        );

        let RyeOsViewVm::Document {
            path,
            content,
            truncated,
            ..
        } = bound_view_vm(&core, tile_id, view_ref)
        else {
            panic!("document source should project as a document primitive")
        };
        assert_eq!(path, "src/main.rs");
        assert_eq!(content, "fn main() {}");
        assert!(truncated);
    }

    #[test]
    fn rows_use_authored_secondary_projection_without_dumping_missing_fields() {
        let mut core = session_with_views(
            json!({"view:test/rows": {
                "widget":"rows", "sources":{"default":{"ref":"service:test/read","collection":"rows"}},
                "projections":{"primary":"title","secondary":"detail","glyph":"icon"}
            }}),
            json!(["view:test/rows"]),
        );
        let key = tile_default_source_key(
            &core,
            core.active_surface_view_sets().unwrap()[0].focused_tile,
        );
        core.data.sources.insert(
            key,
            json!({"rows":[
                {"title":"Project","detail":"Current snapshot","icon":"◇"}, {"title":"No detail"}
            ]}),
        );
        let vm = build_view_model(&core);
        let RyeOsLayoutNodeVm::Tile { view, .. } =
            vm.view_set.as_ref().unwrap().root.clone().unwrap()
        else {
            panic!("tile");
        };
        let RyeOsViewVm::Rows { rows, .. } = *view else {
            panic!("rows");
        };
        assert_eq!(rows[0].secondary.as_deref(), Some("Current snapshot"));
        assert_eq!(rows[1].secondary, None);
        assert_eq!(rows[0].glyph.as_deref(), Some("◇"));
        assert_eq!(rows[1].glyph, None);
    }

    #[test]
    fn authored_supplement_reuses_scene_content_without_creating_actions() {
        let core = session_with_views(
            json!({"view:test/detail": {
                "widget":"rows", "body":{
                    "heading":{"title":"A task", "metadata":["Sample"]},
                    "supplement":{"frame_label":"Detail", "excerpt":[{"field":"1","value":"<script>not markup</script>"}]},
                    "scene":{"objects":[{"position":[0,0,0],"to":[10,10,0]}]}
                }
            }}),
            json!(["view:test/detail"]),
        );
        let instance = core.active_surface_view_sets().unwrap()[0].tiles
            [&core.active_surface_view_sets().unwrap()[0].focused_tile]
            .instance_key
            .clone();
        let heading = view_heading(&core, &instance, "view:test/detail").unwrap();
        assert_eq!(heading.title, "A task");
        let supplement = view_supplement(&core, &instance, "view:test/detail").unwrap();
        assert_eq!(supplement.excerpt[0].value, "<script>not markup</script>");
        let scene = supplement.scene.unwrap();
        assert_eq!(scene.objects[0].end, Some([10.0, 10.0, 0.0]));
        assert!(scene.objects[0].intent.is_none());
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
    fn tile_title_reports_untitled_when_authored_name_absent_or_blank() {
        let absent = session_with_views(
            json!({ "view:ryeos/graph/topology": { "widget": "graph" } }),
            json!(["view:ryeos/graph/topology"]),
        );
        assert_eq!(focused_tile_title(&absent), "Untitled view");

        let blank = session_with_views(
            json!({ "view:ryeos/graph/topology": { "widget": "graph", "name": "  " } }),
            json!(["view:ryeos/graph/topology"]),
        );
        assert_eq!(focused_tile_title(&blank), "Untitled view");
    }

    #[test]
    fn ordinary_mounted_views_use_authored_labels_without_reference_prettification() {
        let core = session_with_views(
            json!({
                "view:ryeos/atlas": { "widget": "atlas", "name": "atlas-slug", "title": "Atlas", "description": "the namespace atlas" },
                "view:ryeos/x/raw": { "widget": "rows" }
            }),
            json!(["view:ryeos/atlas", "view:ryeos/x/raw"]),
        );
        let tiles: Vec<_> = core.active_view_set().unwrap().tiles.values().collect();
        assert_eq!(tiles.len(), 2);
        let atlas = tiles
            .iter()
            .find(|tile| tile.view.view_ref == "view:ryeos/atlas")
            .unwrap();
        let unnamed = tiles
            .iter()
            .find(|tile| tile.view.view_ref == "view:ryeos/x/raw")
            .unwrap();
        assert_ne!(atlas.instance_key, unnamed.instance_key);
        assert_eq!(
            view_title(&core, &atlas.instance_key, &atlas.view.view_ref),
            "Atlas"
        );
        assert_eq!(
            view_title(&core, &unnamed.instance_key, &unnamed.view.view_ref),
            "Untitled view"
        );
        assert_eq!(tile_title(&core, atlas), "Atlas");
        assert_eq!(tile_title(&core, unnamed), "Untitled view");
    }

    #[test]
    fn surface_style_border_flows_into_presentation_chrome() {
        let session = session_with_surface(json!({
            "name": "ryeos-base",
            "style": { "border": "thick" }
        }));
        let mut core = test_support_core(session);
        core.style.border = crate::surface::BorderStyleSpec::Hidden;
        let vm = build_view_model(&core);
        assert_eq!(vm.presentation.chrome.border, "thick");

        // Without an active Surface there is no origin to inherit chrome from,
        // even if transient state still contains a different style.
        let mut empty = RyeOsCore::default();
        empty.style.border = crate::surface::BorderStyleSpec::Hidden;
        let default_vm = build_view_model(&empty);
        assert_eq!(default_vm.presentation.chrome.border, "thin");
        let undeclared = test_support_core(session_with_surface(json!({"name":"No style"})));
        assert_eq!(
            build_view_model(&undeclared).presentation.chrome.border,
            "thin"
        );
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
            "sources": { "default": { "ref": "service:events/chain_replay", "collection": "events" } },
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
        use crate::ui::content::{ViewBinding, project_table, table_columns};
        let binding: ViewBinding = serde_yaml::from_str(include_str!(
            "../../../../../bundles/ryeos-ui/.ai/views/ryeos/threads/list.yaml"
        ))
        .unwrap();
        assert_eq!(binding.widget, "table");
        // The watch dashboard sources the operator-scoped UI-ryeos list,
        // active-first (sort: watch), before the limit.
        let source = binding
            .source("default")
            .expect("threads list has a default source");
        assert_eq!(source.item_ref, "service:ui/ryeos-ui/threads/list");
        assert_eq!(source.params["limit"], 200);
        assert_eq!(source.params["sort"], "watch");
        assert_eq!(source.collection.as_deref(), Some("threads"));
        let columns = table_columns(&binding);
        assert_eq!(
            columns.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
            ["thread", "kind", "item", "project", "status", "created"]
        );
        let rows = project_table(
            &binding,
            &json!({ "threads": [
                { "thread_id": "T-ab", "kind": "directive", "item_ref": "directive:ops/base", "status": "running", "requested_by": "fp:claude", "created_at": "2026-06-29T00:00:00Z", "follow": { "display_state": "resume_queued" } },
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
                "base",
                "node",
                "running",
                "00:00 · 29 Jun"
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
        use crate::ui::content::{ViewBinding, project_section};
        let binding: ViewBinding = serde_yaml::from_str(include_str!(
            "../../../../../bundles/ryeos-ui/.ai/views/ryeos/threads/detail.yaml"
        ))
        .unwrap();
        assert_eq!(binding.widget, "sections");
        // One inspect response feeds every section; each reads a different
        // sub-value of it (thread / result / artifacts / coverage).
        let response = json!({
            "schema_version": "ryeos.thread.inspect.v1",
            "thread": { "thread_id": "T-ab", "item_ref": "directive:ops/base", "status": "running" },
            "result": { "outcome_code": "error", "error": "boom" },
            "artifacts": [ { "artifact_type": "file", "artifact_id": "artifact-one", "metadata": {} } ],
            "children": [ { "item_ref": "directive:ops/child", "status": "completed" } ],
            "outcome_coverage": {"state":"available","reason":"signed result available","thread_status":"running"}
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

        // Artifacts use their owner's ID/type, never a fabricated blob URI.
        let artifacts = project_section(section("Artifacts"), &response);
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].primary, "file");
        assert_eq!(artifacts[0].meta.as_deref(), Some("artifact-one"));
        assert!(
            binding
                .sections
                .iter()
                .all(|section| section.title != "Children"),
            "child hierarchy belongs to its separate execution View"
        );

        let coverage = project_section(section("Outcome coverage"), &response);
        assert_eq!(coverage.len(), 1);
        assert_eq!(coverage[0].primary, "available");
        assert_eq!(coverage[0].meta.as_deref(), Some("signed result available"));
        assert!(
            binding
                .sections
                .iter()
                .all(|section| section.title != "Usage"),
            "inspection must not invent usage absent from its response contract"
        );

        // Re-fetches on the selection facet; each section pulls the selected
        // thread via @facet, so an unset selection SUPPRESSES the fetch (D8)
        // rather than sending a null thread_id to the deny_unknown inspect svc.
        assert_eq!(
            binding.refresh.get("on_facet").and_then(|v| v.as_str()),
            Some("selection")
        );
        assert_eq!(
            binding
                .source(&section("Summary").source_channel)
                .expect("section source")
                .params["thread_id"],
            "@facet:selection.thread_id"
        );
    }

    #[test]
    fn actual_items_space_binding_projects_renamed_and_nested_fields() {
        use crate::ui::content::{ViewBinding, project_table, project_tone, table_columns};
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
            "space": "bundle",
            "trust": { "class": "trusted" }
        });
        let rows = project_table(&binding, &json!({ "items": [record.clone()] }), &columns);
        assert_eq!(rows.len(), 1);
        // The real field is `item_kind` (not `kind`) and trust is a nested
        // object (`trust.class`, not a bare string) — both were silently dead
        // under the rows widget's projection; the table binding resolves them.
        assert_eq!(
            rows[0].cells,
            ["directive:ryeos/ops/base", "directive", "bundle", "trusted"]
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
        let session = session_with_surface(json!({
            "name": "t",
            "tiles": ["view:ryeos/threads/list"],
            "views": {
                "view:ryeos/threads/list": {
                    "widget": "table",
                    "sources": { "default": { "ref": "service:threads/list", "collection": "threads" } },
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
        }));
        let mut core = test_support_core(session);
        let key = tile_default_source_key(&core, core.active_view_set().unwrap().focused_tile);
        core.data.sources.insert(
            key.clone(),
            json!({ "threads": [
                { "thread_id": "T-ab", "status": "running" },
                { "thread_id": "T-cd", "status": "running" }
            ]}),
        );

        fn find_tile_view(node: &RyeOsLayoutNodeVm) -> Option<&RyeOsViewVm> {
            match node {
                RyeOsLayoutNodeVm::Tile { view, .. } => Some(view.as_ref()),
                RyeOsLayoutNodeVm::Split { first, second, .. } => {
                    find_tile_view(first).or_else(|| find_tile_view(second))
                }
            }
        }
        let selected_cells = |core: &RyeOsCore| -> Vec<Vec<String>> {
            let vm = build_view_model(core);
            let root = vm
                .view_set
                .as_ref()
                .unwrap()
                .root
                .clone()
                .expect("layout root");
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
                tile_id: core.active_view_set().unwrap().focused_tile.0.to_string(),
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
        let session = session_with_surface(json!({
            "name": "t",
            "tiles": ["view:ryeos/threads/list"],
            "views": {
                "view:ryeos/threads/list": {
                    "widget": "table",
                    "sources": { "default": { "ref": "service:ui/ryeos-ui/threads/list", "collection": "threads" } },
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
        }));
        let mut core = test_support_core(session);
        let key = tile_default_source_key(&core, core.active_view_set().unwrap().focused_tile);
        core.data.sources.insert(
            key,
            json!({ "threads": [ { "thread_id": "T-ab", "chain_root_id": "T-ab" } ] }),
        );

        let items = available_command_items(&core);
        // The focused row (default cursor 0 = T-ab) exposes a reachable Cancel
        // targeting that specific row.
        let cancel = items
            .iter()
            .find(|i| i.label == "Cancel")
            .expect("cancel overlay item");
        assert!(
            matches!(&cancel.intent,
                RyeOsUiIntent::InvokeAffordance { view_ref, affordance_id, record, .. }
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

    #[test]
    fn captured_dock_row_uses_its_own_view_binding_not_the_active_tile() {
        let mut session = session();
        let root_attachment_id = session.root_attachment_id.clone();
        let shared = session
            .binding_attachments
            .iter_mut()
            .find(|entry| entry.binding_attachment_id == root_attachment_id)
            .unwrap();
        shared.presentation.views.insert(
            "view:ryeos/threads/list".into(),
            json!({
                "widget":"table",
                "sources":{"default":{"ref":"service:ui/ryeos-ui/threads/list","collection":"threads"}},
                "projections":{"columns":[{"label":"thread","field":"thread_id"}]},
                "selection":{"activate":"watch"},
                "affordances":[
                    {"id":"watch","label":"Watch","invoke":{"plane":"ui","facet":"selection","value":{"thread":"{record.thread_id}"}}},
                    {"id":"cancel","label":"Cancel","invoke":{"plane":"rye","ref":"service:commands/submit","args":{"thread_id":"{record.thread_id}","command_type":"cancel"}}}
                ]
            }),
        );
        let mut core = test_support_core(session);
        core.presentation
            .as_mut()
            .unwrap()
            .dock_mut("left")
            .unwrap()
            .visible = true;
        let dock = core.shared_dock_instance_key(RyeOsDockEdge::Left).unwrap();
        assert_ne!(core.mounted_view_ref(&dock), Some("view:test/home"));
        let source_key = dock_default_source_key(&core, RyeOsDockEdge::Left);
        core.data
            .sources
            .insert(source_key, json!({"threads":[{"thread_id":"T-dock"}]}));
        let items = available_command_items_with_focus(&core, Some(&dock));
        assert!(items.iter().any(|item| matches!(&item.intent,
            RyeOsUiIntent::InvokeAffordance { instance_key, view_ref, affordance_id, record }
                if instance_key == &dock
                    && view_ref == "view:ryeos/threads/list"
                    && affordance_id == "cancel"
                    && record["thread_id"] == "T-dock"
        )));
    }

    #[test]
    fn shipped_history_exposes_both_comparison_actions_through_shared_overlay() {
        let history: serde_json::Value = serde_yaml::from_str(include_str!(
            "../../../../../bundles/ryeos-ui/.ai/views/ryeos/threads/history.yaml"
        ))
        .unwrap();
        let session = session_with_surface(json!({
            "name": "t",
            "tiles": ["view:ryeos/threads/history"],
            "views": { "view:ryeos/threads/history": history }
        }));
        let mut core = test_support_core(session);
        let key = tile_default_source_key(&core, core.active_view_set().unwrap().focused_tile);
        core.data.sources.insert(
            key,
            json!({ "threads": [{
                "thread_id": "T-ab",
                "chain_root_id": "T-ab",
                "kind": "graph",
                "item_ref": "graph:test/run",
                "status": "completed",
                "created_at": "2026-08-10T00:00:00Z"
            }] }),
        );

        let items = available_command_items(&core);
        for (label, affordance_id) in [
            ("Compare as left", "compare-left"),
            ("Compare as right", "compare-right"),
        ] {
            let item = items
                .iter()
                .find(|item| item.label == label)
                .unwrap_or_else(|| panic!("missing shared action {label}"));
            assert!(matches!(&item.intent,
                RyeOsUiIntent::InvokeAffordance { view_ref, affordance_id: actual, record, .. }
                    if view_ref == "view:ryeos/threads/history"
                        && actual == affordance_id
                        && record["thread_id"] == "T-ab"));
        }
    }

    /// Build a single focused timeline tile over a chain_replay response, with
    /// the feed point (distance-from-bottom 0) on the newest entry.
    fn feed_core(events: serde_json::Value) -> RyeOsCore {
        let session = session_with_surface(json!({
            "name": "t",
            "tiles": ["view:ryeos/chain/timeline"],
            "views": {
                "view:ryeos/chain/timeline": {
                    "widget": "timeline",
                    "sources": { "default": { "ref": "service:events/chain_replay", "collection": "events" } }
                }
            }
        }));
        let mut core = test_support_core(session);
        let tile_key = core.active_view_set().unwrap().focused_tile.0.to_string();
        let key = tile_default_source_key(&core, core.active_view_set().unwrap().focused_tile);
        core.data
            .sources
            .insert(key.clone(), json!({ "events": events }));
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetTileCursor {
                tile_id: tile_key,
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

        let items = available_command_items(&core);
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
        let items = available_command_items(&core);
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
        let items = available_command_items(&core);
        assert!(items.iter().any(|i| i.label == "Inspect selection"));
        assert!(!items.iter().any(|i| i.label == "Retry failed turn"));
    }

    #[test]
    fn append_live_delta_adds_trailing_cursor_block_for_head_thread() {
        let mut core =
            input_session(json!({"widget":"text","input":{"id":"line","submit":"route"}}));
        set_focused_route_value(&mut core, json!({ "thread": "T-1" }));
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
        let mut core =
            input_session(json!({"widget":"text","input":{"id":"line","submit":"route"}}));
        set_focused_route_value(&mut core, json!({ "thread": "T-1" }));
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
        let mut core =
            input_session(json!({"widget":"text","input":{"id":"line","submit":"route"}}));
        set_focused_route_value(&mut core, json!({ "thread": "T-1" }));
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
        let mut core =
            input_session(json!({"widget":"text","input":{"id":"line","submit":"route"}}));
        set_focused_route_value(&mut core, json!({ "thread": "T-1" }));
        core.data.threads = Some(crate::ui::dto::RyeOsThreadsDto {
            threads: vec![json!({ "thread_id": "T-1", "status": "completed" })],
        });

        let mut entries = Vec::new();
        append_live_delta(&core, &mut entries);
        assert!(entries.is_empty(), "settled head → no working indicator");
    }

    #[test]
    fn timeline_entries_unknown_role_degrades_to_line() {
        let binding: super::super::content::ViewBinding = serde_json::from_value(json!({
            "widget": "timeline",
            "sources": { "default": { "ref": "service:events/chain_replay", "collection": "events" } },
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
        let session = session_with_surface_and_docks(
            json!({
                "name": "t",

                "views": { "view:ryeos/input": view }
            }),
            serde_json::json!({ "bottom": { "content": "view:ryeos/input", "open": true, "size": 7 } }),
        );
        let mut core = test_support_core(session);
        core.focused_input = core.shared_dock_instance_key(RyeOsDockEdge::Bottom);
        core
    }

    #[test]
    fn input_view_renders_prompt_in_bottom_slot() {
        let core = input_session(json!({
            "widget": "text",
            "input": { "id": "line", "placeholder": "Ask or run a command", "submit": "route" }
        }));
        let vm = build_view_model(&core);
        let bottom = vm.docks.bottom.expect("bottom slot");
        let input = bottom.input.expect("bottom instance declares input");
        assert_eq!(input.placeholder, "Ask or run a command");
    }

    #[test]
    fn view_set_vm_serializes_stable_view_instance_keys() {
        let mut core = test_support_core(session_with_surface(json!({"name":"Empty"})));
        let presentation_id = core.presentation.as_ref().unwrap().id().clone();
        let surface_id = core.active_surface().unwrap().id().clone();
        let tile_id = core
            .active_view_set_mut()
            .unwrap()
            .add_tile(
                crate::view_set::ViewSpec::bound("view:test/missing"),
                &presentation_id,
                &surface_id,
            )
            .expect("fixture layout accepts view");
        let vm = build_view_model(&core);
        let root = vm
            .view_set
            .as_ref()
            .unwrap()
            .root
            .clone()
            .expect("mounted center tile");
        let RyeOsLayoutNodeVm::Tile { instance_key, .. } = root else {
            panic!("one tile renders as a leaf");
        };
        assert_eq!(
            instance_key,
            core.active_view_set().unwrap().tiles[&tile_id].instance_key
        );
        assert_eq!(
            serde_json::to_value(instance_key).unwrap(),
            json!(
                core.active_view_set().unwrap().tiles[&tile_id]
                    .instance_key
                    .as_str()
            )
        );
    }

    #[test]
    fn target_label_override_wins_over_derived_strip() {
        let core = input_session(json!({
            "widget": "text",
            "input": { "id": "line", "target_label": "thread input", "submit": "route" }
        }));
        let vm = build_view_model(&core);
        let input = vm.docks.bottom.unwrap().input.unwrap();
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
            crate::ui::source_key::RyeOsSourceInstanceKey::completion(
                core.shared_dock_instance_key(RyeOsDockEdge::Bottom).unwrap(),
                "line",
            )
            .encode(),
            json!({
                "commands": [
                    { "invocable": true, "tokens": ["thread", "list"], "description": "List threads" },
                    { "invocable": true, "tokens": ["thread", "get"], "description": "Get thread" }
                ]
            }),
        );
        *core
            .focused_input_buffer_mut()
            .expect("input buffer exists") = crate::ui::model::RyeOsInputState {
            text: "/thread ".to_string(),
            cursor: "/thread ".len(),
            ..Default::default()
        };
        let vm = build_view_model(&core);
        let input = vm.docks.bottom.unwrap().input.unwrap();
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
            crate::ui::source_key::RyeOsSourceInstanceKey::completion(
                core.shared_dock_instance_key(RyeOsDockEdge::Bottom)
                    .unwrap(),
                "line",
            )
            .encode(),
            json!({ "commands": [
                { "invocable": true, "tokens": ["thread", "list"] }
            ] }),
        );
        *core
            .focused_input_buffer_mut()
            .expect("input buffer exists") = crate::ui::model::RyeOsInputState {
            text: "/thr".to_string(),
            cursor: 4,
            ..Default::default()
        };
        let vm = build_view_model(&core);
        let input = vm.docks.bottom.unwrap().input.unwrap();
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
            crate::ui::source_key::RyeOsSourceInstanceKey::mention(
                core.shared_dock_instance_key(RyeOsDockEdge::Bottom)
                    .unwrap(),
                "line",
            )
            .encode(),
            json!({ "threads": [
                { "thread_id": "T-ab", "item_ref": "directive:ops/base" },
                { "thread_id": "T-cd", "item_ref": "directive:demo/chat" }
            ]}),
        );
        *core
            .focused_input_buffer_mut()
            .expect("input buffer exists") = crate::ui::model::RyeOsInputState {
            text: "ping @directive".to_string(),
            cursor: "ping @directive".len(),
            ..Default::default()
        };
        let vm = build_view_model(&core);
        let input = vm.docks.bottom.unwrap().input.unwrap();
        assert!(
            input
                .completion
                .iter()
                .any(|s| s.contains("directive:ops/base") && s.contains("directive:demo/chat")),
            "mention hint lists matching refs by label: {:?}",
            input.completion
        );
    }

    #[test]
    fn launcher_projects_typed_source_and_destination_transfer_choices() {
        let mut session = crate::ui::reducer::test_support::session();
        let mut destination = session.presentations[0].clone();
        destination.presentation_id = "22222222-2222-4222-8222-222222222222".into();
        destination.thread_id = Some("destination-seat".into());
        destination.chain_root_id = Some("destination-root".into());
        session.presentations.push(destination);
        let mut core = test_support_core(session);
        claim_fixture_producer(&mut core);
        let choices = available_command_items(&core);
        assert!(choices.iter().any(|choice| matches!(
            &choice.intent,
            RyeOsUiIntent::BeginTransfer {
                mode: crate::ui::transfer_export::UiTransferExportMode::Move,
                source: crate::ui::event::UiTransferSourceSelection::SurfaceViewSets { .. },
                destination_presentation_id,
            } if destination_presentation_id == "22222222-2222-4222-8222-222222222222"
        )));
        assert!(!choices.iter().any(|choice| matches!(
            &choice.intent,
            RyeOsUiIntent::BeginTransfer { destination_presentation_id, .. }
                if destination_presentation_id == FIXTURE_PRESENTATION_ID
        )));
        assert!(choices.iter().any(|choice| {
            matches!(
                &choice.intent,
                RyeOsUiIntent::BeginTransfer {
                    mode: crate::ui::transfer_export::UiTransferExportMode::Move,
                    ..
                }
            ) && choice
                .label
                .contains("22222222-2222-4222-8222-222222222222")
                && choice.hint.contains("acknowledged freeze")
        }));
        assert!(choices.iter().any(|choice| matches!(
            &choice.intent,
            RyeOsUiIntent::BeginTransfer {
                mode: crate::ui::transfer_export::UiTransferExportMode::Copy,
                ..
            }
        ) && choice.hint.contains("fresh required inputs")));
    }

    #[test]
    fn launcher_new_document_uses_exact_root_without_replacing_current_producer() {
        let session = crate::ui::reducer::test_support::session();
        let mut core = test_support_core(session);
        core.supports_independent_documents = true;
        claim_fixture_producer(&mut core);
        let producer = core.presentation_operations.producer.clone();
        let choice = available_command_items(&core)
            .into_iter()
            .find(|choice| matches!(&choice.intent, RyeOsUiIntent::OpenNewPresentationDocument))
            .expect("new document action is reachable");
        assert!(choice.enabled);
        let effects = core.dispatch_intent(choice.intent);
        assert!(matches!(effects.as_slice(), [effect] if matches!(
            &effect.kind,
            crate::ui::effect::RyeOsEffectKind::OpenNewPresentationDocument {
                expected_session_id,
                root_binding,
            } if expected_session_id == &core.data.session.as_ref().unwrap().session_id
                && root_binding == core.presentation.as_ref().unwrap().root_binding()
        )));
        assert_eq!(core.presentation_operations.producer, producer);
    }

    #[test]
    fn new_document_action_survives_zero_surfaces() {
        let mut core = test_support_core(shared_session());
        core.supports_independent_documents = true;
        claim_fixture_producer(&mut core);
        assert!(core.active_surface().is_none());
        assert!(available_command_items(&core).iter().any(|choice| {
            choice.enabled && matches!(&choice.intent, RyeOsUiIntent::OpenNewPresentationDocument)
        }));
    }

    #[test]
    fn unsupported_document_host_disables_projection_and_direct_intent() {
        let mut core = test_support_core(crate::ui::reducer::test_support::session());
        claim_fixture_producer(&mut core);
        assert!(!core.supports_independent_documents);
        let choice = available_command_items(&core)
            .into_iter()
            .find(|choice| matches!(choice.intent, RyeOsUiIntent::OpenNewPresentationDocument))
            .unwrap();
        assert!(!choice.enabled);
        assert!(choice.hint.contains("does not support"));
        let producer = core.presentation_operations.producer.clone();
        assert!(
            core.dispatch_intent(RyeOsUiIntent::OpenNewPresentationDocument)
                .is_empty()
        );
        assert_eq!(core.presentation_operations.producer, producer);
        assert!(!core.ui.notices.is_empty());
        assert!(
            available_command_items(&core)
                .iter()
                .any(|choice| choice.label == "New view set" && choice.enabled)
        );
    }

    #[test]
    fn start_replacement_preserves_document_host_implementation_fact() {
        for supported in [false, true] {
            let mut core = test_support_core(shared_session());
            claim_fixture_producer(&mut core);
            core.supports_independent_documents = supported;
            let session = core.data.session.clone().unwrap();
            let presentation_id = core
                .presentation_operations
                .producer
                .as_ref()
                .unwrap()
                .presentation_id
                .clone();
            let viewport = core.runtime.viewport;
            let now_ms = core.runtime.now_ms;
            core.dispatch(crate::ui::event::RyeOsEvent::Start {
                session,
                presentation_id,
                viewport,
                now_ms,
            });
            assert_eq!(core.supports_independent_documents, supported);
            assert!(
                core.presentation_operations.producer.is_none(),
                "Start must actually replace the producer-bearing Core"
            );
        }
    }
}
