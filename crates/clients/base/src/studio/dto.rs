//! Studio daemon DTOs.
//!
//! These structs model the JSON returned by the current daemon UI endpoints
//! without making those endpoint names part of the Studio product model.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How the daemon delivered a `service:threads/input` submit. Typed so the
/// client branches on a variant, not a string literal. Unknown/future values
/// fold to `Unknown` (treated as non-launched).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadDelivery {
    Launched,
    Submitted,
    Refused,
    #[serde(other)]
    Unknown,
}

/// A control command submitted for an existing thread via
/// `service:commands/submit`. Typed so the client emits a variant, not a string
/// literal; serializes to the daemon's accepted `command_type` vocabulary
/// (validated daemon-side in `command_service`). Note: interrupting a *running*
/// directive is NOT one of these — that is a text-bearing live redirect via
/// `service:threads/input` (the foot input / Alt+Enter), not a control command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadControlCommand {
    Continue,
    Cancel,
    Kill,
    Interrupt,
}

impl ThreadControlCommand {
    /// The wire `command_type` spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Cancel => "cancel",
            Self::Kill => "kill",
            Self::Interrupt => "interrupt",
        }
    }
}

/// A thread's lifecycle status as it arrives on the wire. Mirrors the substrate
/// status vocabulary (the daemon is the source of truth); typed here so UI code
/// classifies by variant rather than matching raw status strings scattered in
/// logic. Unknown/future values fold to `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    Created,
    Running,
    Completed,
    Failed,
    Cancelled,
    Killed,
    TimedOut,
    Continued,
    #[serde(other)]
    Unknown,
}

impl ThreadStatus {
    /// Parse the wire spelling; unrecognized → `Unknown`. The one boundary
    /// where a status string becomes a variant (mirrors the substrate enum's
    /// own `from_str_lossy`).
    pub fn from_wire(status: &str) -> Self {
        match status {
            "created" => Self::Created,
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "killed" => Self::Killed,
            "timed_out" => Self::TimedOut,
            "continued" => Self::Continued,
            _ => Self::Unknown,
        }
    }
}

/// The fields the braid lens reads from a `cognition_out` event payload. Typed
/// so the projection branches on a field rather than a raw JSON key. Other
/// payload fields (turn, tokens, content, tool_calls) reach the feed through the
/// event projection, not this struct, so they are intentionally omitted.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CognitionOutPayload {
    /// The cognition was cut mid-flight by a live interrupt; the braid marks the
    /// seam where it was cut and the next `cognition_in` folds.
    #[serde(default)]
    pub interrupted: bool,
}

/// Daemon-authored per-execution facts, surfaced both on thread projections
/// (`thread.execution`) and on a continuation launch result — the substrate
/// authority the client gates machine-continuation (`supports_continuation`) and
/// operator-input (`supports_operator_followup`) affordances on. Mirrors the
/// daemon `ExecutionFacts`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct ExecutionFacts {
    #[serde(default)]
    pub supports_continuation: bool,
    /// `false` for machine-only kinds (graph): the kind is continuation-capable
    /// but folds no conversation, so the operator-input affordance must gate on
    /// this, not on `supports_continuation` alone.
    #[serde(default)]
    pub supports_operator_followup: bool,
}

/// Daemon-authored graph follow-lineage fact, surfaced on a thread projection's
/// `follow` field when the thread participates in a `follow:` relationship —
/// either the suspended parent awaiting a child chain, or the resume successor
/// that consumes the child's result. Instance-derived (distinct from the
/// kind-derived [`ExecutionFacts`]); absent (`None` on the row) for non-follow
/// threads. Mirrors the daemon `FollowFact` wire shape exactly.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct FollowFact {
    /// [`follow_role::SUSPENDED_PARENT`] or [`follow_role::RESUME_SUCCESSOR`].
    #[serde(default)]
    pub role: String,
    /// Live waiter phase (`waiting`/`ready`/`resuming`); `suspended_parent` only.
    #[serde(default)]
    pub phase: Option<String>,
    /// The graph node id that issued the follow.
    #[serde(default)]
    pub follow_node: Option<String>,
    /// The followed child chain's head thread.
    #[serde(default)]
    pub child_thread_id: Option<String>,
    /// The followed child chain's root id.
    #[serde(default)]
    pub child_chain_root_id: Option<String>,
    /// The child chain's terminal status once known; `None` while still running.
    #[serde(default)]
    pub child_terminal_status: Option<String>,
    /// The parent's resume-successor thread id.
    #[serde(default)]
    pub parent_successor_thread_id: Option<String>,
}

/// The `follow.role` wire strings a client branches on. Kept in sync with the
/// daemon `thread_lifecycle::follow_role`.
pub mod follow_role {
    pub const SUSPENDED_PARENT: &str = "suspended_parent";
    pub const RESUME_SUCCESSOR: &str = "resume_successor";
}

impl FollowFact {
    /// This thread issued a follow and is suspended (`continued`) awaiting its
    /// child chain — never a valid operator-input target while suspended.
    pub fn is_suspended_parent(&self) -> bool {
        self.role == follow_role::SUSPENDED_PARENT
    }

    /// This thread is the parent's resume successor (consumes the child result).
    pub fn is_resume_successor(&self) -> bool {
        self.role == follow_role::RESUME_SUCCESSOR
    }
}

/// The typed result of a `service:threads/input` submit
/// (`{ thread_id?, delivery, notice?, pending?, execution? }`). A non-launch
/// invocation (e.g. a slash command) deserializes to all-default — `delivery`
/// absent.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LaunchOutcome {
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub delivery: Option<ThreadDelivery>,
    #[serde(default)]
    pub notice: Option<String>,
    /// Staged-input depth after an accepted live steer (`delivery: submitted`) —
    /// the count of operator inputs queued behind the not-yet-folded ones.
    /// Absent on launch/refuse outcomes.
    #[serde(default)]
    pub pending: Option<u64>,
    /// Present on a continuation launch (kind known synchronously); absent on a
    /// fresh async launch (the thread is created later).
    #[serde(default)]
    pub execution: Option<ExecutionFacts>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioDimensionDto {
    #[serde(default)]
    pub schema_version: String,
    #[serde(default)]
    pub generated_at: String,
    #[serde(default)]
    pub session: StudioSessionDto,
    #[serde(default)]
    pub local_node: StudioLocalNodeDto,
    #[serde(default)]
    pub project: Option<StudioProjectDto>,
    #[serde(default)]
    pub remotes: Vec<StudioRemoteDto>,
    #[serde(default)]
    pub threads: StudioThreadSummaryDto,
    #[serde(default)]
    pub schedules: StudioScheduleSummaryDto,
    #[serde(default)]
    pub gc: StudioGcSummaryDto,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioSessionDto {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub surface_ref: String,
    #[serde(default)]
    pub user_principal_id: Option<String>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub granted_caps: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioLocalNodeDto {
    #[serde(default)]
    pub identity: StudioIdentityDto,
    #[serde(default)]
    pub status: serde_json::Value,
    #[serde(default)]
    pub health: serde_json::Value,
    #[serde(default)]
    pub spaces: Vec<StudioSpaceDto>,
    #[serde(default)]
    pub bundles: Vec<StudioBundleDto>,
    #[serde(default)]
    pub services: Vec<StudioServiceDto>,
    #[serde(default)]
    pub commands: Vec<StudioCommandDto>,
    #[serde(default)]
    pub command_aliases: Vec<StudioCommandDto>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioIdentityDto {
    #[serde(default)]
    pub principal_id: String,
    #[serde(default)]
    pub fingerprint: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioSpaceDto {
    #[serde(default)]
    pub space: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioBundleDto {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioServiceDto {
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub service_ref: String,
    #[serde(default)]
    pub availability: String,
    #[serde(default)]
    pub required_caps: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioCommandDto {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioProjectDto {
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioProjectsDto {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub projects: Vec<StudioKnownProjectDto>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioKnownProjectDto {
    #[serde(default)]
    pub local_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub added_at: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub exists: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioAddProjectDto {
    #[serde(default)]
    pub project: StudioKnownProjectDto,
    #[serde(default)]
    pub created: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioOpenProjectDto {
    #[serde(default)]
    pub project: StudioKnownProjectDto,
    #[serde(default)]
    pub session: StudioOpenProjectSessionDto,
    #[serde(default)]
    pub recent: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioOpenProjectSessionDto {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub project_root: Option<String>,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioRemoteDto {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub principal_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioThreadSummaryDto {
    #[serde(default)]
    pub active_count: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioScheduleSummaryDto {
    #[serde(default)]
    pub total: usize,
    #[serde(default)]
    pub enabled: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioGcSummaryDto {
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub recent_events: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioTopologyDto {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
    #[serde(default)]
    pub nodes: Vec<StudioTopologyNodeDto>,
    #[serde(default)]
    pub edges: Vec<StudioTopologyEdgeDto>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioTopologyNodeDto {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub label: String,
    #[serde(default, rename = "ref")]
    pub ref_: String,
    #[serde(default)]
    pub space: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default, rename = "virtual")]
    pub virtual_: bool,
    #[serde(default)]
    pub missing: bool,
    #[serde(default)]
    pub status: Option<StudioTopologyNodeStatusDto>,
    #[serde(default)]
    pub trust: Option<StudioTopologyTrustSummaryDto>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioTopologyNodeStatusDto {
    #[serde(default)]
    pub resolved: bool,
    #[serde(default)]
    pub composed: Option<bool>,
    #[serde(default)]
    pub executable: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioTopologyTrustSummaryDto {
    #[serde(default, rename = "class")]
    pub class_: String,
    #[serde(default)]
    pub signer: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioTopologyEdgeDto {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
    #[serde(default, rename = "type")]
    pub type_: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub source: Option<StudioTopologyEdgeSourceDto>,
    #[serde(default)]
    pub confidence: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioTopologyEdgeSourceDto {
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioItemsDto {
    #[serde(default)]
    pub schema_version: String,
    #[serde(default)]
    pub counts: StudioItemCountsDto,
    #[serde(default)]
    pub items: Vec<StudioItemDto>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioItemCountsDto {
    #[serde(default)]
    pub by_kind: BTreeMap<String, usize>,
    #[serde(default)]
    pub by_space: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioItemDto {
    #[serde(default)]
    pub canonical_ref: String,
    #[serde(default)]
    pub item_kind: String,
    #[serde(default)]
    pub bare_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub space: String,
    #[serde(default)]
    pub source_path: String,
    #[serde(default)]
    pub executable: bool,
    #[serde(default)]
    pub trust: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioThreadsDto {
    #[serde(default)]
    pub threads: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioFilesDto {
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub entries: Vec<StudioFileEntryDto>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioFileEntryDto {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub is_dir: bool,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub modified: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioFileSpaceDto {
    #[serde(default)]
    pub schema_version: String,
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub max_depth: usize,
    #[serde(default)]
    pub max_entries: usize,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub watchable: bool,
    #[serde(default)]
    pub supports_expand: bool,
    #[serde(default)]
    pub ignore_mode: String,
    #[serde(default)]
    pub entries: Vec<StudioFileSpaceEntryDto>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioFileSpaceEntryDto {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub is_dir: bool,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub modified: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioFileReadDto {
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub size: usize,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StudioRawContentDto {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub bytes: usize,
    #[serde(default)]
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_status_parses_serdes_and_folds_unknown() {
        // from_wire: exact substrate spellings, and a multi-word one.
        assert_eq!(ThreadStatus::from_wire("timed_out"), ThreadStatus::TimedOut);
        assert_eq!(ThreadStatus::from_wire("running"), ThreadStatus::Running);
        // Unrecognized folds to Unknown (both via from_wire and serde `other`).
        assert_eq!(ThreadStatus::from_wire("nonsense"), ThreadStatus::Unknown);
        assert_eq!(
            serde_json::from_value::<ThreadStatus>(serde_json::json!("nonsense")).unwrap(),
            ThreadStatus::Unknown
        );
        // snake_case round-trip on the wire spelling.
        assert_eq!(
            serde_json::to_value(ThreadStatus::TimedOut).unwrap(),
            serde_json::json!("timed_out")
        );
        assert_eq!(
            serde_json::from_value::<ThreadStatus>(serde_json::json!("timed_out")).unwrap(),
            ThreadStatus::TimedOut
        );
    }

    #[test]
    fn follow_fact_deserializes_daemon_suspended_parent_shape() {
        // The exact wire shape the daemon emits on a `continued` follow parent.
        let row = serde_json::json!({
            "role": "suspended_parent",
            "phase": "waiting",
            "follow_node": "n_follow",
            "child_thread_id": "T-child",
            "child_chain_root_id": "T-child",
            "child_terminal_status": null,
            "parent_successor_thread_id": "T-succ"
        });
        let f: FollowFact = serde_json::from_value(row).unwrap();
        assert!(f.is_suspended_parent());
        assert!(!f.is_resume_successor());
        assert_eq!(f.phase.as_deref(), Some("waiting"));
        assert_eq!(f.follow_node.as_deref(), Some("n_follow"));
        assert_eq!(f.child_chain_root_id.as_deref(), Some("T-child"));
        assert!(f.child_terminal_status.is_none());
        assert_eq!(f.parent_successor_thread_id.as_deref(), Some("T-succ"));
    }

    #[test]
    fn follow_fact_deserializes_minimal_resume_successor_shape() {
        // The waiter-cleared durable form: only role + successor identity.
        let row = serde_json::json!({
            "role": "resume_successor",
            "child_terminal_status": null,
            "parent_successor_thread_id": "T-succ"
        });
        let f: FollowFact = serde_json::from_value(row).unwrap();
        assert!(f.is_resume_successor());
        assert!(f.phase.is_none(), "resume_successor carries no phase");
        assert!(f.follow_node.is_none());
        assert!(f.child_thread_id.is_none());
    }

    #[test]
    fn launch_outcome_reads_pending_on_submitted() {
        let resp = serde_json::json!({
            "thread_id": "T-1",
            "delivery": "submitted",
            "notice": "Input queued (2 staged).",
            "pending": 2,
            "execution": { "supports_continuation": true, "supports_operator_followup": true }
        });
        let out: LaunchOutcome = serde_json::from_value(resp).unwrap();
        assert_eq!(out.delivery, Some(ThreadDelivery::Submitted));
        assert_eq!(out.pending, Some(2));
        // A launch/refuse outcome without the field stays None.
        let bare: LaunchOutcome = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(bare.pending, None);
    }
}
