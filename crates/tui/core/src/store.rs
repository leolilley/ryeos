//! Normalized store — canonical model of daemon/project facts.

use crate::ids::{ProjectId, RemoteId, ThreadId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

impl ThreadModel {
    pub fn new(id: ThreadId) -> Self {
        Self {
            id,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[derive(Default)]
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

/// Type alias matching spec.
pub type ThreadIdType = ThreadId;
