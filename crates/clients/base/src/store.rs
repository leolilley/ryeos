//! Normalized store — canonical model of daemon/project facts.

use crate::ids::{ProjectId, RemoteId, ThreadId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Operational cockpit snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitSnapshotModel {
    pub schema_version: String,
    pub generated_at: String,
    pub session: SessionModel,
    pub local_node: LocalNodeModel,
    pub project: Option<ProjectInfoModel>,
    pub schedules: ScheduleSummaryModel,
    pub gc: GcSummaryModel,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScheduleListModel {
    pub schedules: Vec<ScheduleModel>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScheduleModel {
    pub schedule_id: String,
    pub item_ref: String,
    pub schedule_type: String,
    pub expression: String,
    pub timezone: Option<String>,
    pub enabled: bool,
    pub last_fire_at: Option<i64>,
    pub last_fire_status: Option<String>,
    pub total_fires: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GcStatusModel {
    pub running: bool,
    pub state: Option<serde_json::Value>,
    pub recent_events: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileBrowserModel {
    pub root: String,
    pub path: String,
    pub truncated: bool,
    pub entries: Vec<FileEntryModel>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileEntryModel {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileReadModel {
    pub root: String,
    pub path: String,
    pub size: usize,
    pub truncated: bool,
    pub content: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionModel {
    pub session_id: String,
    pub surface_ref: String,
    pub read_only: bool,
    pub granted_caps: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalNodeModel {
    pub identity: IdentityInfoModel,
    pub health_status: String,
    pub operational_services: String,
    pub missing_services: Vec<String>,
    pub spaces: Vec<SpaceSummaryModel>,
    pub bundles: Vec<BundleSummaryModel>,
    pub services: Vec<ServiceSummaryModel>,
    pub commands: Vec<CommandSummaryModel>,
    pub command_aliases: Vec<CommandSummaryModel>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdentityInfoModel {
    pub principal_id: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpaceSummaryModel {
    pub space: String,
    pub label: String,
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BundleSummaryModel {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceSummaryModel {
    pub endpoint: String,
    pub service_ref: String,
    pub availability: String,
    pub required_caps: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommandSummaryModel {
    pub name: String,
    pub target: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectInfoModel {
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScheduleSummaryModel {
    pub total: usize,
    pub enabled: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GcSummaryModel {
    pub running: bool,
    pub recent_event_count: usize,
}

// ---------------------------------------------------------------------------
// Thread
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ThreadStatus {
    Created,
    Running,
    Completed,
    Failed,
    Cancelled,
    Killed,
    TimedOut,
    Continued,
}

impl ThreadStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ThreadStatus::Completed
                | ThreadStatus::Failed
                | ThreadStatus::Cancelled
                | ThreadStatus::Killed
                | ThreadStatus::TimedOut
        )
    }

    pub fn is_running(&self) -> bool {
        matches!(self, ThreadStatus::Running)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreadUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub spend_usd: f64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadPart {
    pub kind: ThreadPartKind,
    pub text: String,
    pub tool_name: Option<String>,
    pub child_thread_id: Option<ThreadId>,
    pub duration_ms: Option<u64>,
    pub tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ThreadPartKind {
    UserMessage,
    AssistantMessage,
    Thinking,
    ToolCall,
    ToolResult,
    ChildThread,
    System,
    Context,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadModel {
    pub id: ThreadId,
    pub daemon_id: Option<String>,
    pub status: ThreadStatus,
    pub item_ref: Option<String>,
    pub parent_id: Option<ThreadId>,
    pub children: Vec<ThreadId>,
    pub started_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub usage: ThreadUsage,
    pub parts: Vec<ThreadPart>,
    pub streaming_text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreadInspectionModel {
    pub thread_id: String,
    pub status: String,
    pub item_ref: String,
    pub kind: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub children: Vec<serde_json::Value>,
    pub events: Vec<serde_json::Value>,
    pub result: Option<serde_json::Value>,
    pub artifacts: Vec<serde_json::Value>,
    pub facets: Option<serde_json::Value>,
}

impl ThreadModel {
    pub fn new(id: ThreadId) -> Self {
        Self {
            id,
            daemon_id: None,
            status: ThreadStatus::Created,
            item_ref: None,
            parent_id: None,
            children: Vec::new(),
            started_at_ms: None,
            completed_at_ms: None,
            usage: ThreadUsage::default(),
            parts: Vec::new(),
            streaming_text: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityModel {
    pub fingerprint: String,
    pub has_signing_key: bool,
}

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum DaemonStatus {
    #[default]
    Connecting,
    Connected,
    Disconnected,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonModel {
    pub status: DaemonStatus,
    pub url: String,
    pub uptime_secs: Option<u64>,
    pub active_threads: u32,
}

// ---------------------------------------------------------------------------
// Remote
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RemoteSyncState {
    Synced,
    Ahead,
    Behind,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteModel {
    pub id: RemoteId,
    pub name: String,
    pub url: String,
    pub alive: bool,
    pub last_seen_ms: Option<i64>,
    pub sync_state: RemoteSyncState,
    pub capabilities: Vec<String>,
    pub trust_fingerprint: String,
    pub trust_pinned: bool,
}

// ---------------------------------------------------------------------------
// Project
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ItemCounts {
    pub directives: u32,
    pub tools: u32,
    pub knowledge: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectModel {
    pub id: ProjectId,
    pub path: String,
    pub name: String,
    pub item_counts: ItemCounts,
}

// ---------------------------------------------------------------------------
// Budget
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BudgetModel {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_spend_usd: f64,
}

// ---------------------------------------------------------------------------
// Trust
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustAlert {
    pub message: String,
    pub severity: TrustSeverity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TrustSeverity {
    Info,
    Warning,
    Error,
}

// ---------------------------------------------------------------------------
// Event record
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub event_type: String,
    pub timestamp_ms: i64,
    pub payload: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    pub identity: Option<IdentityModel>,
    pub daemon: DaemonModel,
    pub cockpit: Option<CockpitSnapshotModel>,
    pub schedules: ScheduleListModel,
    pub gc_status: Option<GcStatusModel>,
    pub files: Option<FileBrowserModel>,
    pub file_read: Option<FileReadModel>,
    pub thread_inspection: Option<ThreadInspectionModel>,
    pub item_inspection: Option<ItemInspectionModel>,
    pub threads: HashMap<ThreadId, ThreadModel>,
    pub remotes: HashMap<RemoteId, RemoteModel>,
    pub projects: HashMap<ProjectId, ProjectModel>,
    pub items: HashMap<ItemId, ItemModel>,
    pub events: Vec<EventRecord>,
    pub budget: BudgetModel,
    pub trust_alerts: Vec<TrustAlert>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of threads currently running.
    pub fn running_thread_count(&self) -> usize {
        self.threads
            .values()
            .filter(|t| t.status.is_running())
            .count()
    }

    /// Get threads sorted by start time (newest first).
    pub fn recent_threads(&self) -> Vec<&ThreadModel> {
        let mut threads: Vec<_> = self.threads.values().collect();
        threads.sort_by(|a, b| {
            b.started_at_ms
                .unwrap_or(0)
                .cmp(&a.started_at_ms.unwrap_or(0))
        });
        threads
    }
}

/// Lightweight item reference.
pub type ItemId = crate::ids::ItemId;

/// Lightweight item model for space browsing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemModel {
    pub id: ItemId,
    pub kind: String,
    pub name: String,
    pub category: Option<String>,
    pub description: Option<String>,
    pub signed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ItemInspectionModel {
    pub canonical_ref: String,
    pub item_kind: String,
    pub source_path: String,
    pub space: String,
    pub raw_content: Option<String>,
    pub raw_truncated: bool,
    pub effective: Option<serde_json::Value>,
}

/// Type alias matching spec.
pub type ThreadIdType = ThreadId;
