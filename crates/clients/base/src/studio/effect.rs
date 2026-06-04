use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioEffect {
    pub id: u64,
    pub kind: StudioEffectKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StudioEffectKind {
    FetchSnapshot,
    FetchProjects,
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
    Snapshot,
    Projects,
    ProjectAdded,
    ProjectOpened,
    Threads,
    Items,
    Schedules,
    GcStatus,
    FilesList,
    FileRead,
    ItemInspection,
    ThreadInspection,
    BrowserOnly,
}
