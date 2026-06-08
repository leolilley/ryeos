use serde::{Deserialize, Serialize};

use super::model::StudioInputRoute;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioEffect {
    pub id: u64,
    pub kind: StudioEffectKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StudioEffectKind {
    FetchDimension,
    FetchProjects,
    FetchTopology,
    AddProject {
        root: String,
    },
    OpenProject {
        local_id: String,
    },
    FetchThreads {
        limit: usize,
    },
    FetchItems {
        tile_id: Option<String>,
        query: Option<String>,
        kind: Option<String>,
        limit: usize,
    },
    FetchSchedules,
    FetchGcStatus,
    ListFiles {
        tile_id: Option<String>,
        root: String,
        path: String,
    },
    FetchFileSpace {
        root: String,
        path: String,
        max_depth: usize,
        max_entries: usize,
    },
    ReadFile {
        root: String,
        path: String,
    },
    InspectItem {
        canonical_ref: String,
        include_raw: bool,
        include_effective: bool,
    },
    InspectThread {
        thread_id: String,
        event_limit: usize,
    },
    InvokeAction {
        command_id: String,
        args: serde_json::Value,
    },
    CancelThread {
        thread_id: String,
    },
    SubmitInput {
        route: StudioInputRoute,
        text: String,
    },
    SetLocationHash {
        hash: String,
    },
    CopyToClipboard {
        text: String,
    },
    OpenUrl {
        url: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioEffectResult {
    pub id: u64,
    pub ok: bool,
    pub kind: StudioEffectResultKind,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StudioEffectResultKind {
    Dimension,
    Projects,
    Topology,
    ProjectAdded,
    ProjectOpened,
    Threads,
    Items,
    Schedules,
    GcStatus,
    FilesList,
    FileSpace,
    FileRead,
    ItemInspection,
    ThreadInspection,
    ActionInvocation,
    ThreadCancelled,
    InputSubmitted,
    BrowserOnly,
}
