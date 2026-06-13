use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    /// Generic source fetch for a bound view: ONE mechanism for all
    /// content-defined tiles. `{ref, params} -> result keyed to the
    /// subscribing tile`.
    FetchSource {
        tile_id: String,
        source_ref: String,
        params: Value,
    },
    /// Command records for completion (the grammar shown is the grammar
    /// held — records carry per-session invocability, evaluated
    /// daemon-side).
    FetchCommands,
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
    InvokeAction {
        command_id: String,
        args: serde_json::Value,
    },
    CancelThread {
        thread_id: String,
    },
    /// THE generic rye-plane invocation. The client never interprets the
    /// target; the substrate decides. `route_seq` carries the seat-braid
    /// seq of `input.route` at issue time when the invocation came from
    /// the routed input — results arriving after a later route event may
    /// notice but never retarget.
    Invoke {
        target: InvokeRef,
        params: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        route_seq: Option<u64>,
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
    FilesList,
    FileSpace,
    FileRead,
    ActionInvocation,
    ThreadCancelled,
    Invoked,
    Commands,
    SourceData,
    BrowserOnly,
}

/// Target forms for the generic invocation: a canonical item ref, or
/// command tokens resolved/bound daemon-side (token dispatch lands with
/// the one-daemon-path slice).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "form", rename_all = "snake_case")]
pub enum InvokeRef {
    Ref { item_ref: String },
    Tokens { tokens: Vec<String> },
}
