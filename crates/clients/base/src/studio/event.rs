use serde::{Deserialize, Serialize};

use super::effect::StudioEffectResult;
use super::model::{BrowserSession, BrowserViewport, StudioDockEdge};
use crate::atlas::{AtlasItemKind, AtlasLensVm, AtlasProjectionVm};
use crate::workspace::{FocusDirection, ViewSpec};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StudioFilterField {
    ItemsQuery,
    ItemsKind,
    ServicesQuery,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StudioAction {
    Refresh,
    /// Run a content-declared affordance against a projected row: the
    /// ONE generic row interaction. The engine resolves the binding's
    /// affordance, substitutes row fields, and applies its plane (ui
    /// facet write or rye token dispatch). No product verbs in code.
    InvokeAffordance {
        view_ref: String,
        affordance_id: String,
        record: serde_json::Value,
    },
    OpenView {
        view: ViewSpec,
    },
    OpenNewView {
        view: ViewSpec,
    },
    CloseFocused,
    CloseTile {
        tile_id: String,
    },
    ToggleFocusedMaster,
    MoveFocusedTile {
        direction: StudioStackMoveDirection,
    },
    CycleTab {
        direction: StudioStackMoveDirection,
    },
    SwitchTab {
        index: usize,
    },
    ToggleTopStatusBar,
    ToggleBottomStatusBar,
    ToggleDock {
        edge: StudioDockEdge,
    },
    ResizeFocused {
        direction: FocusDirection,
    },
    SelectDimension,
    InspectItem {
        canonical_ref: String,
    },
    EnterItemFolder {
        tile_id: String,
        path: String,
    },
    InspectThread {
        thread_id: String,
    },
    InspectSummary {
        title: String,
        detail: serde_json::Value,
    },
    AddCurrentProject,
    OpenProject {
        local_id: String,
    },
    ListFiles {
        tile_id: String,
        root: String,
        path: String,
    },
    ReadFile {
        root: String,
        path: String,
    },
    CopyText {
        text: String,
    },
    OpenExternal {
        url: String,
    },
    ExecuteItem {
        item_ref: String,
        parameters: serde_json::Value,
    },
    CancelThread {
        thread_id: String,
    },
    /// Steer the route's head thread via `service:commands/submit`
    /// (`cancel` / `interrupt` / `continue` / `kill`). The reducer reads the
    /// head thread at dispatch time. This is the same authority the CLI's
    /// `ryeos commands submit` uses — no new bypass; see the daemon authz
    /// note at `command_service.submit` / `.tmp/thread-authorization-review.md`.
    SubmitThreadCommand {
        command: String,
    },
    /// Aim the input route at a thread — the feed re-projects to its braid.
    /// Activating a forked-subthread feed entry "enters" that subthread.
    AimThread {
        thread_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StudioUiEvent {
    Activate {
        action: StudioAction,
    },
    SetFilter {
        tile_id: String,
        field: StudioFilterField,
        value: String,
    },
    SetFilesRoot {
        tile_id: String,
        root: String,
    },
    SetFilesPath {
        tile_id: String,
        path: String,
    },
    SetAtlasLayerVisible {
        /// Target tile; `None` (default) targets the ambient backdrop atlas.
        #[serde(default)]
        tile_id: Option<String>,
        kind: AtlasItemKind,
        visible: bool,
    },
    SetAtlasLens {
        #[serde(default)]
        tile_id: Option<String>,
        lens: AtlasLensVm,
    },
    SetAtlasProjection {
        #[serde(default)]
        tile_id: Option<String>,
        projection: AtlasProjectionVm,
        #[serde(default)]
        root: Option<String>,
    },
    SetAtlasFileSpacePath {
        #[serde(default)]
        tile_id: Option<String>,
        root: String,
        path: String,
    },
    FocusChanged {
        target: Option<String>,
    },
    FocusDirection {
        direction: FocusDirection,
    },
    OpenLauncher,
    CloseLauncher,
    SetLauncherQuery {
        query: String,
    },
    InsertInputChar {
        ch: char,
    },
    DeleteInputChar,
    SetInputText {
        text: String,
        cursor: usize,
    },
    CompleteInput,
    SubmitInput,
    MoveLauncherSelection {
        delta: i32,
    },
    ChooseLauncher {
        secondary: bool,
    },
    SetTileCursor {
        tile_id: String,
        index: usize,
    },
    /// Fold (`collapsed: true`) or unfold a turn-section of a feed lens.
    SetFold {
        tile_id: String,
        section: usize,
        collapsed: bool,
    },
    ActivateFocused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StudioStackMoveDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StudioEvent {
    Start {
        session: BrowserSession,
        viewport: BrowserViewport,
        now_ms: u64,
    },
    Ui {
        event: StudioUiEvent,
    },
    EffectResult {
        result: StudioEffectResult,
    },
    DaemonEvent {
        payload: serde_json::Value,
    },
    /// One frame from the head thread's live SSE tail. The reducer applies
    /// ryeos event semantics so both clients reach them through `dispatch`:
    /// cognition deltas accumulate into the live buffer; durable milestones
    /// supersede it and refetch the braid snapshot. Clients only open the
    /// stream and forward each frame's `(event_type, payload)`.
    ThreadTail {
        thread_id: String,
        event_type: String,
        payload: serde_json::Value,
    },
    Tick {
        now_ms: u64,
    },
    Resize {
        viewport: BrowserViewport,
    },
    RouteChanged {
        route: String,
    },
}
