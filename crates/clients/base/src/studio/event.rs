use serde::{Deserialize, Serialize};

use super::effect::StudioEffectResult;
use super::model::{BrowserSession, BrowserViewport};
use crate::layout::SplitAxis;
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
    OpenView {
        view: ViewSpec,
    },
    OpenNewView {
        view: ViewSpec,
    },
    SplitFocused {
        axis: SplitAxis,
    },
    SplitTile {
        tile_id: String,
        axis: SplitAxis,
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
    ResizeFocused {
        direction: FocusDirection,
    },
    SelectSnapshot,
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
