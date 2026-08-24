use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::launch_metadata::{LAUNCH_METADATA_SCHEMA_VERSION, RuntimeLaunchMetadata};
use crate::process::{
    ExecutionProcessIdentity, PROCESS_IDENTITY_SCHEMA_VERSION,
    validate_execution_process_identity_shape,
};

const MAX_DEDICATED_SESSION_COMMANDS: i64 = 100_000;
const MAX_DEDICATED_SESSION_COMMAND_SPOOL_BYTES: i64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopIntent {
    Cancel,
    Kill,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlanningRecord {
    pub launch_id: String,
    pub reserved_thread_id: String,
    pub requested_by: String,
    pub daemon_generation_id: String,
    pub state: String,
    pub bound_thread_id: Option<String>,
    pub outcome_code: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("pending launch planning admission reached its bounded capacity")]
pub struct LaunchPlanningCapacityExceeded;

#[derive(Debug, thiserror::Error)]
#[error("launch planning coordinate is already reserved")]
pub struct LaunchPlanningAlreadyReserved;

/// Result of recording one operational parent/child lineage edge.
///
/// Exact replays are expected when a durable launch is re-driven. A child is
/// nevertheless allowed to have only one operational parent and relation, so a
/// conflicting replay is an integrity error rather than another idempotent hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildLinkInsertOutcome {
    Inserted,
    AlreadyPresent,
}

/// Durable stop behavior coupled to a child-link write.
///
/// Keeping this policy inside the runtime database transaction closes the
/// crash window between making a late child reachable and tombstoning it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildLinkStopPolicy {
    None,
    Always(StopIntent),
    IfInserted(StopIntent),
}

impl StopIntent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cancel => "cancel",
            Self::Kill => "kill",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "cancel" => Ok(Self::Cancel),
            "kill" => Ok(Self::Kill),
            other => bail!("invalid durable stop intent `{other}`"),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RuntimeInfo {
    pub pid: Option<i64>,
    pub pgid: Option<i64>,
    /// Internal signal authority. Never expose boot IDs/start ticks through a
    /// service response; callers only need the existing pid/pgid accounting.
    #[serde(skip_serializing)]
    pub process_identity: Option<ExecutionProcessIdentity>,
    #[serde(skip_serializing)]
    pub process_dead_observed_at_ms: Option<i64>,
    #[serde(skip_serializing)]
    pub stop_requested_at_ms: Option<i64>,
    #[serde(skip_serializing)]
    pub stop_intent: Option<StopIntent>,
    /// Internal recovery/resume authority. It can retain the original free-form
    /// execution parameters, so it must never be echoed through ThreadDetail or
    /// another service response. Internal owners use `get_launch_metadata`.
    #[serde(skip_serializing)]
    pub launch_metadata: Option<RuntimeLaunchMetadata>,
    /// Outer-only classification of an authority contract that is not the
    /// exact current wire schema. The predecessor payload remains opaque in
    /// SQLite and is never deserialized into current authority.
    #[serde(skip_serializing)]
    pub incompatible_launch_metadata: Option<IncompatibleLaunchMetadata>,
    pub recovery_wait: Option<RecoveryWaitDisposition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncompatibleLaunchMetadata {
    pub schema_version: u64,
    pub admitted_launch_capsule_schema: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InProcessHandlerReservationPhase {
    Pending,
    Running,
    TerminalConfirmed,
}

impl InProcessHandlerReservationPhase {
    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "terminal_confirmed" => Ok(Self::TerminalConfirmed),
            _ => bail!("invalid in-process handler reservation phase `{raw}`"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InProcessHandlerReservation {
    pub thread_id: String,
    pub phase: InProcessHandlerReservationPhase,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RuntimeThreadHistoryDiscardReport {
    pub thread_runtime: usize,
    pub in_process_handler_reservations: usize,
    pub thread_commands: usize,
    pub hook_dispatch_ledger: usize,
    pub detached_spawn_intents: usize,
    pub recovery_waits: usize,
    pub thread_launch_claims: usize,
    pub thread_launch_epochs: usize,
    pub execution_workspaces: usize,
    pub follow_waiters: usize,
    pub follow_waiter_children: usize,
    pub thread_child_links: usize,
    pub launch_windows: usize,
    pub seat_leases: usize,
    pub launch_planning: usize,
}

impl RuntimeThreadHistoryDiscardReport {
    pub fn total_rows(&self) -> usize {
        self.thread_runtime
            + self.in_process_handler_reservations
            + self.thread_commands
            + self.hook_dispatch_ledger
            + self.detached_spawn_intents
            + self.recovery_waits
            + self.thread_launch_claims
            + self.thread_launch_epochs
            + self.execution_workspaces
            + self.follow_waiters
            + self.follow_waiter_children
            + self.thread_child_links
            + self.launch_windows
            + self.seat_leases
            + self.launch_planning
    }
}

/// Runtime-owned facts which can make an otherwise terminal chain unsafe to
/// retire.  This is deliberately structural: retention callers never infer
/// safety from an item kind or ref, and a failed inspection propagates as an
/// error (therefore pins the chain).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ChainRecoveryPins {
    /// Runtime rows whose chain membership disagrees with authoritative chain
    /// truth. These are retained as a structural pin rather than silently
    /// orphaned by deleting the signed head.
    pub runtime_membership_conflicts: u64,
    pub in_process_handler_reservations: u64,
    pub live_processes: u64,
    pub launch_claims: u64,
    /// Active launch claims whose persisted launch contract is resume- or
    /// continuation-capable. This is deliberately derived from an owning claim;
    /// a non-zero historical `resume_attempts` counter is not an in-flight owner.
    pub recovery_capable_launch_claims: u64,
    /// Durable owners which may still consume a checkpoint. Checkpoint files and
    /// launch metadata alone are residue, not pins: an owning recovery launch or
    /// parent follow waiter must still exist.
    pub required_checkpoint_consumers: u64,
    pub pending_commands: u64,
    /// Open cancel/kill commands or cancelled launch-window tombstones which
    /// still require the recovery/cascade machinery to converge.
    pub cancellation_repairs: u64,
    pub follow_waiters: u64,
    pub launch_windows: u64,
    pub seat_leases: u64,
    pub child_links: u64,
    pub scheduler_fires: u64,
}

impl ChainRecoveryPins {
    pub fn is_empty(&self) -> bool {
        self.runtime_membership_conflicts == 0
            && self.in_process_handler_reservations == 0
            && self.live_processes == 0
            && self.launch_claims == 0
            && self.recovery_capable_launch_claims == 0
            && self.required_checkpoint_consumers == 0
            && self.pending_commands == 0
            && self.cancellation_repairs == 0
            && self.follow_waiters == 0
            && self.launch_windows == 0
            && self.seat_leases == 0
            && self.child_links == 0
            && self.scheduler_fires == 0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ThreadRecoveryOwners {
    recovery_capable_launch_claims: u64,
    required_checkpoint_consumers: u64,
    cancellation_repairs: u64,
}

/// Classify recovery ownership from durable state-machine rows rather than
/// from historical counters or leftover files. A launch contract is only live
/// for retention while its launch claim exists. Likewise, a checkpoint path is
/// only required while that claimed recovery-capable launch can consume it.
fn classify_thread_recovery_owners(
    runtime_info: Option<&RuntimeInfo>,
    launch_claims: u64,
    open_control_commands: u64,
) -> ThreadRecoveryOwners {
    let metadata = runtime_info.and_then(|info| info.launch_metadata.as_ref());
    let recovery_capable = metadata.is_some_and(|metadata| {
        metadata.native_resume.is_some() || metadata.resume_context.is_some()
    });
    let claimed_recovery = if recovery_capable { launch_claims } else { 0 };
    let claimed_checkpoint_consumer = if metadata.is_some_and(|metadata| {
        metadata.checkpoint_dir.is_some()
            && (metadata.native_resume.is_some() || metadata.resume_context.is_some())
    }) {
        launch_claims
    } else {
        0
    };
    ThreadRecoveryOwners {
        recovery_capable_launch_claims: claimed_recovery,
        required_checkpoint_consumers: claimed_checkpoint_consumer,
        cancellation_repairs: open_control_commands,
    }
}

fn add_pin_count(total: &mut u64, count: u64, label: &str) -> Result<()> {
    *total = total
        .checked_add(count)
        .with_context(|| format!("{label} recovery-pin count overflow"))?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct CommandRecord {
    pub command_id: i64,
    pub thread_id: String,
    pub command_type: String,
    pub status: String,
    pub requested_by: Option<String>,
    pub params: Option<Value>,
    pub result: Option<Value>,
    pub created_at: String,
    pub claimed_at: Option<String>,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewCommandRecord {
    pub thread_id: String,
    pub command_type: String,
    pub requested_by: Option<String>,
    pub params: Option<Value>,
}

/// Maximum JSON size of one command's params at durable admission.
pub const MAX_COMMAND_PARAMS_BYTES: usize = 256 * 1024;
/// Maximum JSON size of one command's terminal result at durable admission.
pub const MAX_COMMAND_RESULT_BYTES: usize = MAX_COMMAND_PARAMS_BYTES;
/// Maximum UTF-8 size of the optional command requester identity.
pub const MAX_COMMAND_REQUESTED_BY_BYTES: usize = 4 * 1024;
/// Maximum number of pending commands transitioned by one runtime claim.
pub const MAX_COMMAND_CLAIM_ITEMS: usize = 32;
/// Exact serialized command-result budget, below the 10 MiB UDS frame limit.
pub const MAX_COMMAND_CLAIM_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const CONTINUATION_SEED_MARKER: &[u8] = b"continuation_seed";
pub(crate) const CONTINUATION_SEED_RECONCILE_PAGE_SIZE: usize = 512;
pub const IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE: usize = 512;
pub const MAX_IN_PROCESS_HANDLER_RESERVATIONS: usize = 4_096;

/// A live thread cannot accumulate unbounded terminalization work.
pub const MAX_OPEN_COMMANDS_PER_THREAD: usize = 128;
/// Aggregate variable content retained by a thread's open commands.
pub const MAX_OPEN_COMMAND_CONTENT_BYTES: usize = 4 * 1024 * 1024;

/// Maximum exact callback response retained for a completed hook dispatch.
/// This remains below the callback UDS frame budget and prevents the ledger
/// from becoming an unbounded response store.
pub const MAX_HOOK_DISPATCH_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Exact dispatch-identity seed admitted by this runtime-store epoch.
pub const HOOK_DISPATCH_SEED_VERSION: u32 = 3;

#[derive(Debug, Clone)]
pub struct NewHookDispatch {
    pub seed_version: u32,
    pub dispatch_key: String,
    pub chain_root_id: String,
    pub caller_thread_id: String,
    pub event: String,
    pub hook_id: String,
    pub request_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookDispatchStatus {
    Pending,
    Completed,
}

impl HookDispatchStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Completed => "completed",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "completed" => Ok(Self::Completed),
            other => bail!("invalid hook dispatch status `{other}`"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum HookDispatchReservation {
    Execute,
    Replay(CompletedHookDispatch),
    PendingUnknown,
}

#[derive(Debug, Clone)]
pub struct CompletedHookDispatch {
    pub dispatch_key: String,
    pub chain_root_id: String,
    pub occurrence_thread_id: String,
    pub event: String,
    pub hook_id: String,
    pub request_hash: String,
    pub response: Value,
    pub response_hash: String,
}

#[derive(Debug, Clone)]
pub struct DetachedSpawnIntent {
    pub operation_id: String,
    pub parent_thread_id: String,
    pub request_hash: String,
    pub child_thread_id: String,
    pub child_project_authority: Option<ryeos_state::objects::ExecutionProjectAuthority>,
    pub admitted_launch_capsule_hash: Option<String>,
    pub launch_metadata: Option<crate::launch_metadata::RuntimeLaunchMetadata>,
    pub incompatible_launch_metadata: Option<IncompatibleLaunchMetadata>,
    pub initial_events: Option<Vec<crate::state_store::NewEventRecord>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecoveryWaitDisposition {
    pub thread_id: String,
    pub reason: String,
    pub detail: String,
    pub started_at_ms: i64,
    pub deadline_at_ms: i64,
}

fn decode_detached_spawn_intent(row: &rusqlite::Row<'_>) -> rusqlite::Result<DetachedSpawnIntent> {
    Ok(DetachedSpawnIntent {
        operation_id: row.get(0)?,
        parent_thread_id: row.get(1)?,
        request_hash: row.get(2)?,
        child_thread_id: row.get(3)?,
        child_project_authority: row
            .get::<_, Option<String>>(4)?
            .map(|raw| decode_current_project_authority_column(4, &raw))
            .transpose()?,
        admitted_launch_capsule_hash: row.get(5)?,
        launch_metadata: row
            .get::<_, Option<String>>(6)?
            .map(|raw| decode_stored_launch_metadata_column(6, &raw))
            .transpose()?
            .and_then(StoredLaunchMetadata::current),
        incompatible_launch_metadata: row
            .get::<_, Option<String>>(6)?
            .map(|raw| decode_stored_launch_metadata_column(6, &raw))
            .transpose()?
            .and_then(StoredLaunchMetadata::incompatible),
        initial_events: row
            .get::<_, Option<String>>(7)?
            .map(|raw| serde_json::from_str(&raw))
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    7,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?,
    })
}

fn validate_runtime_thread_id(thread_id: &str) -> Result<()> {
    if thread_id.is_empty()
        || thread_id.trim() != thread_id
        || thread_id.len() > 256
        || thread_id.chars().any(char::is_control)
    {
        bail!("runtime thread id is not canonical");
    }
    Ok(())
}

fn validate_bounded_runtime_text(label: &str, value: &str, max_bytes: usize) -> Result<()> {
    if value.is_empty()
        || value.trim() != value
        || value.len() > max_bytes
        || value.chars().any(char::is_control)
    {
        bail!("{label} is not canonical or exceeds {max_bytes} bytes");
    }
    Ok(())
}

/// Validate the closed command vocabulary at the durable database boundary.
///
/// Service callers use this same policy for an early error, but every direct
/// database caller is still required to cross this admission check.
pub fn validate_command_type(command_type: &str) -> Result<()> {
    match command_type {
        "cancel" | "kill" | "interrupt" | "continue" => Ok(()),
        other => bail!("invalid command_type: {other}"),
    }
}

const BOUNDED_COMMAND_SELECT: &str = "SELECT command_id, thread_id, command_type, status, \
            CASE WHEN requested_by IS NULL OR length(CAST(requested_by AS BLOB)) <= ?1 \
                 THEN requested_by ELSE NULL END AS requested_by, \
            CASE WHEN params IS NULL OR length(params) <= ?2 THEN params ELSE NULL END AS params, \
            CASE WHEN result IS NULL OR length(result) <= ?3 THEN result ELSE NULL END AS result, \
            created_at, claimed_at, completed_at, \
            length(CAST(requested_by AS BLOB)) AS requested_by_len, \
            length(params) AS params_len, length(result) AS result_len \
     FROM thread_commands";

/// Outcome of attempting to claim the right to launch a thread.
///
/// The launch claim is the ONLY thing that authorizes a spawn and the only way
/// to distinguish an **unlaunched** successor (no claim / expired claim) from one
/// **mid-launch** (a live claim held by some launcher). It is keyed on
/// `thread_id`, so at most one launcher owns a thread's launch window at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchClaimOutcome {
    /// The caller now owns this thread's launch window (fresh claim, or a stale
    /// lease reclaimed). The caller's `claim_id` is recorded.
    Claimed,
    /// Another launcher holds an unexpired claim — back off, do not spawn.
    AlreadyClaimed,
}

/// One dead-generation launch claim removed by the startup sweep.
#[derive(Debug, Clone)]
pub struct StaleLaunchClaimCleared {
    pub thread_id: String,
    pub claim_id: String,
    pub dead_generation: String,
}

/// A live launch claim, as read back for reconcile/inspection.
#[derive(Debug, Clone)]
pub struct LaunchClaim {
    pub thread_id: String,
    pub claim_id: String,
    pub claimed_at_ms: i64,
    pub lease_expires_at_ms: i64,
    pub claimed_by: String,
    pub owner: LaunchOwner,
}

/// Canonical durable fencing identity for one launch attempt. The JSON form is
/// stored in the existing `claimed_by` column so the cutover does not weaken
/// the runtime database's exact-schema ownership checks.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchOwner {
    pub thread_id: String,
    pub monotonic_launch_epoch: u64,
    pub unpredictable_nonce: String,
    pub daemon_generation_id: String,
}

pub fn daemon_generation_id() -> &'static str {
    static GENERATION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    GENERATION.get_or_init(|| {
        format!(
            "daemon-{}-{}",
            std::process::id(),
            crate::thread_lifecycle::new_thread_id()
        )
    })
}

#[derive(Debug, Clone)]
pub struct WorkspaceRecord {
    pub workspace_id: String,
    pub thread_id: Option<String>,
    pub launch_owner: Option<String>,
    pub backend_id: Option<String>,
    pub backend_version: Option<String>,
    pub pinned_root_identities: Option<String>,
    pub mount_identity: Option<String>,
    pub lower_snapshot: String,
    /// Post-freeze snapshot bound atomically with the owning thread's
    /// ResumeContext. Present only once a `freezing` workspace has a durable
    /// generation from which recovery may continue.
    pub frozen_snapshot_hash: Option<String>,
    pub root_path: String,
    pub state: WorkspaceState,
    pub process_identity: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Complete durable evidence published when a constructed workspace becomes
/// ready. Grouping these same-shaped optional values prevents call-site
/// argument reordering while preserving the journal's explicit nullable
/// representation for pre-bind recovery.
pub struct WorkspaceBinding<'a> {
    pub workspace_id: &'a str,
    pub thread_id: &'a str,
    pub launch_owner: Option<&'a str>,
    pub backend_id: Option<&'a str>,
    pub backend_version: Option<&'a str>,
    pub pinned_root_identities: Option<&'a str>,
    pub mount_identity: Option<&'a str>,
}

/// Canonical durable execution-workspace lifecycle. SQLite stores the stable
/// snake-case spelling, while every Rust reader and transition uses this type
/// so an unknown state fails at the persistence boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceState {
    Reserved,
    Constructing,
    Ready,
    Active,
    Freezing,
    Destroying,
    Closing,
    Closed,
    Orphaned,
}

impl WorkspaceState {
    pub const ALL: [Self; 9] = [
        Self::Reserved,
        Self::Constructing,
        Self::Ready,
        Self::Active,
        Self::Freezing,
        Self::Destroying,
        Self::Closing,
        Self::Closed,
        Self::Orphaned,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Constructing => "constructing",
            Self::Ready => "ready",
            Self::Active => "active",
            Self::Freezing => "freezing",
            Self::Destroying => "destroying",
            Self::Closing => "closing",
            Self::Closed => "closed",
            Self::Orphaned => "orphaned",
        }
    }
}

impl std::fmt::Display for WorkspaceState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug)]
pub struct WorkspaceStateParseError(String);

impl std::fmt::Display for WorkspaceStateParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "unknown execution workspace state {:?}", self.0)
    }
}

impl std::error::Error for WorkspaceStateParseError {}

impl std::str::FromStr for WorkspaceState {
    type Err = WorkspaceStateParseError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "reserved" => Ok(Self::Reserved),
            "constructing" => Ok(Self::Constructing),
            "ready" => Ok(Self::Ready),
            "active" => Ok(Self::Active),
            "freezing" => Ok(Self::Freezing),
            "destroying" => Ok(Self::Destroying),
            "closing" => Ok(Self::Closing),
            "closed" => Ok(Self::Closed),
            "orphaned" => Ok(Self::Orphaned),
            other => Err(WorkspaceStateParseError(other.to_owned())),
        }
    }
}

impl rusqlite::types::FromSql for WorkspaceState {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|error| rusqlite::types::FromSqlError::Other(Box::new(error)))
    }
}

/// Phase of a follow waiter. The row exists only while the follow is active —
/// `clear_follow_waiter` deletes it once the parent successor is independently
/// recoverable. EVERY stored phase is recoverable by reconcile.
pub mod follow_phase {
    pub const RESERVED: &str = "reserved";
    pub const WAITING: &str = "waiting";
    pub const READY: &str = "ready";
    pub const RESUMING: &str = "resuming";
}

/// Fields needed to reserve a follow attempt (the get-or-create seed).
#[derive(Debug, Clone)]
pub struct NewFollowWaiter {
    pub follow_key: String,
    pub parent_thread_id: String,
    pub parent_chain_root_id: String,
    pub follow_node: String,
    pub graph_run_id: String,
    pub step_count: i64,
    pub frontier_id: Option<String>,
    pub fanout: bool,
    pub expected_children: u32,
    pub child_project_authority: Option<ryeos_state::objects::ExecutionProjectAuthority>,
}

#[derive(Debug, Clone)]
pub struct FollowWaiterChild {
    pub item_index: u32,
    pub item_ref: String,
    pub spec_hash: String,
    pub child_thread_id: String,
    pub child_chain_root_id: String,
    pub sealed_root_request: crate::thread_lifecycle::SealedRootExecutionRequest,
    pub terminal_thread_id: Option<String>,
    pub terminal_status: Option<String>,
    pub terminal_envelope: Option<Value>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Stable identity for one normalized follow child specification. An idempotent
/// re-drive can never adopt different execution identity, parameters, or facets
/// at an already-recorded cohort index.
pub fn follow_child_spec_hash(
    item_ref: &str,
    ref_bindings: &BTreeMap<String, String>,
    parameters: &Value,
    facets: Option<&Value>,
) -> Result<String> {
    let spec = serde_json::json!({
        "item_ref": item_ref,
        "ref_bindings": ref_bindings,
        "parameters": parameters,
        "facets": facets.cloned().unwrap_or(Value::Null),
    });
    let canonical = lillux::canonical_json(&spec)
        .context("failed to canonicalize normalized follow child specification")?;
    Ok(lillux::sha256_hex(canonical.as_bytes()))
}

/// A durable parent↔child follow dependency. The graph checkpoint owns the
/// parent's cursor; this waiter owns the successor and cohort contract, while
/// its ordered child rows own child identities and terminal envelopes. Keyed by `follow_key`
/// (`parent_thread_id`/`graph_run_id`/`follow_node`/`step_count`), which is the
/// idempotency key for the whole follow attempt.
#[derive(Debug, Clone)]
pub struct FollowWaiter {
    pub follow_key: String,
    pub parent_thread_id: String,
    pub parent_chain_root_id: String,
    pub parent_successor_thread_id: Option<String>,
    pub follow_node: String,
    pub graph_run_id: String,
    pub step_count: i64,
    pub frontier_id: Option<String>,
    pub fanout: bool,
    pub expected_children: u32,
    pub child_project_authority: Option<ryeos_state::objects::ExecutionProjectAuthority>,
    pub children: Vec<FollowWaiterChild>,
    pub phase: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// The bounded, response-facing projection of a live follow waiter.
///
/// Thread lists need lineage plus cohort progress, not the child terminal
/// envelopes used by reconciliation. Keeping this separate prevents a list
/// page from loading arbitrary result JSON out of `follow_waiter_child`.
#[derive(Debug, Clone)]
pub struct FollowWaiterSummary {
    pub follow_key: String,
    pub parent_thread_id: String,
    pub parent_successor_thread_id: Option<String>,
    pub follow_node: String,
    pub phase: String,
    pub fanout: bool,
    pub expected_children: u32,
    pub first_child_thread_id: Option<String>,
    pub first_child_chain_root_id: Option<String>,
    pub first_child_terminal_status: Option<String>,
    pub child_count: u32,
    pub terminal_child_count: u32,
    pub created_at_ms: i64,
}

impl FollowWaiterSummary {
    pub fn all_children_terminal(&self) -> bool {
        self.expected_children > 0
            && self.child_count == self.expected_children
            && self.terminal_child_count == self.expected_children
    }
}

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS thread_runtime (
    thread_id TEXT PRIMARY KEY,
    chain_root_id TEXT NOT NULL,
    pid INTEGER,
    pgid INTEGER,
    metadata BLOB,
    launch_metadata TEXT,
    resume_attempts INTEGER NOT NULL DEFAULT 0,
    process_identity TEXT,
    process_dead_observed_at_ms INTEGER,
    stop_requested_at_ms INTEGER,
    stop_intent TEXT
);

CREATE INDEX IF NOT EXISTS idx_thread_runtime_chain_root
    ON thread_runtime(chain_root_id);

CREATE TABLE IF NOT EXISTS in_process_handler_reservation (
    thread_id TEXT PRIMARY KEY,
    phase TEXT NOT NULL CHECK (phase IN ('pending', 'running', 'terminal_confirmed')),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_in_process_handler_reservation_phase_thread
    ON in_process_handler_reservation(phase, thread_id);

CREATE TABLE IF NOT EXISTS thread_commands (
    command_id INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id TEXT NOT NULL,
    command_type TEXT NOT NULL,
    status TEXT NOT NULL,
    requested_by TEXT,
    params BLOB,
    result BLOB,
    created_at TEXT NOT NULL,
    claimed_at TEXT,
    completed_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_thread_commands_thread_status
    ON thread_commands(thread_id, status);

CREATE TABLE IF NOT EXISTS hook_dispatch_ledger (
    dispatch_key TEXT PRIMARY KEY,
    seed_version INTEGER NOT NULL CHECK (seed_version = 3),
    chain_root_id TEXT NOT NULL,
    caller_thread_id TEXT NOT NULL,
    event TEXT NOT NULL,
    hook_id TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'completed')),
    response_json BLOB,
    response_hash TEXT,
    created_at_ms INTEGER NOT NULL,
    completed_at_ms INTEGER,
    CHECK (
        (status = 'pending' AND response_json IS NULL AND response_hash IS NULL AND completed_at_ms IS NULL)
        OR
        (status = 'completed' AND response_json IS NOT NULL AND response_hash IS NOT NULL AND completed_at_ms IS NOT NULL)
    )
);

CREATE INDEX IF NOT EXISTS idx_hook_dispatch_ledger_chain_root
    ON hook_dispatch_ledger(chain_root_id);

CREATE TABLE IF NOT EXISTS detached_spawn_intent (
    operation_id TEXT PRIMARY KEY,
    parent_thread_id TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    child_thread_id TEXT NOT NULL UNIQUE,
    child_project_authority TEXT,
    admitted_launch_capsule_hash TEXT,
    launch_metadata TEXT,
    initial_events TEXT,
    created_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS thread_recovery_wait (
    thread_id TEXT PRIMARY KEY,
    reason TEXT NOT NULL,
    detail TEXT NOT NULL,
    started_at_ms INTEGER NOT NULL,
    deadline_at_ms INTEGER NOT NULL,
    CHECK (deadline_at_ms > started_at_ms)
);

CREATE TABLE IF NOT EXISTS thread_launch_claim (
    thread_id TEXT PRIMARY KEY,
    claim_id TEXT NOT NULL,
    claimed_at_ms INTEGER NOT NULL,
    lease_expires_at_ms INTEGER NOT NULL,
    claimed_by TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS thread_launch_epoch (
    thread_id TEXT PRIMARY KEY,
    last_epoch INTEGER NOT NULL CHECK (last_epoch > 0)
);

CREATE TABLE IF NOT EXISTS execution_workspace (
    workspace_id TEXT PRIMARY KEY,
    thread_id TEXT,
    launch_owner TEXT,
    backend_id TEXT,
    lower_snapshot TEXT NOT NULL,
    frozen_snapshot_hash TEXT,
    root_path TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('reserved', 'constructing', 'ready', 'active', 'freezing', 'destroying', 'closing', 'closed', 'orphaned')),
    process_identity TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    backend_version TEXT,
    pinned_root_identities TEXT,
    mount_identity TEXT
);

CREATE INDEX IF NOT EXISTS idx_execution_workspace_thread
    ON execution_workspace(thread_id);

CREATE TABLE IF NOT EXISTS follow_waiter (
    follow_key TEXT PRIMARY KEY,
    parent_thread_id TEXT NOT NULL,
    parent_chain_root_id TEXT NOT NULL,
    parent_successor_thread_id TEXT,
    follow_node TEXT NOT NULL,
    graph_run_id TEXT NOT NULL,
    step_count INTEGER NOT NULL,
    frontier_id TEXT,
    phase TEXT NOT NULL CHECK (phase IN ('reserved', 'waiting', 'ready', 'resuming')),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    fanout INTEGER NOT NULL DEFAULT 0,
    expected_children INTEGER NOT NULL DEFAULT 1,
    child_project_authority TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_follow_waiter_successor
    ON follow_waiter(parent_successor_thread_id);

CREATE TABLE IF NOT EXISTS follow_waiter_child (
    follow_key TEXT NOT NULL,
    item_index INTEGER NOT NULL,
    item_ref TEXT NOT NULL,
    spec_hash TEXT NOT NULL,
    child_thread_id TEXT NOT NULL,
    child_chain_root_id TEXT NOT NULL,
    sealed_root_request TEXT NOT NULL,
    terminal_thread_id TEXT,
    terminal_status TEXT,
    terminal_envelope TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (follow_key, item_index)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_follow_waiter_child_chain2
    ON follow_waiter_child(child_chain_root_id);

CREATE TABLE IF NOT EXISTS thread_child_link (
    child_thread_id TEXT PRIMARY KEY,
    parent_thread_id TEXT NOT NULL,
    relation TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_thread_child_link_parent
    ON thread_child_link(parent_thread_id);

CREATE TABLE IF NOT EXISTS launch_window (
    child_chain_root_id TEXT PRIMARY KEY,
    window_key TEXT NOT NULL,
    width INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    launched_at_ms INTEGER,
    cancelled_at_ms INTEGER
);

CREATE INDEX IF NOT EXISTS idx_launch_window_key
    ON launch_window(window_key);

CREATE TABLE IF NOT EXISTS seat_lease (
    seat_thread_id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    surface TEXT NOT NULL,
    client_ref TEXT NOT NULL,
    last_seen_at_ms INTEGER NOT NULL,
    reaping_at_ms INTEGER
);

CREATE INDEX IF NOT EXISTS idx_seat_lease_last_seen
    ON seat_lease(last_seen_at_ms);

CREATE TABLE IF NOT EXISTS launch_planning (
    launch_id TEXT PRIMARY KEY,
    reserved_thread_id TEXT NOT NULL UNIQUE,
    requested_by TEXT NOT NULL,
    daemon_generation_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('planning', 'bound', 'cancelled', 'failed', 'expired')),
    bound_thread_id TEXT,
    outcome_code TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    finished_at_ms INTEGER,
    CHECK (
        (state = 'planning' AND bound_thread_id IS NULL AND outcome_code IS NULL AND finished_at_ms IS NULL)
        OR
        (state = 'bound' AND bound_thread_id IS NOT NULL AND bound_thread_id = reserved_thread_id AND outcome_code IS NOT NULL AND finished_at_ms IS NOT NULL)
        OR
        (state IN ('cancelled', 'failed', 'expired') AND bound_thread_id IS NULL AND outcome_code IS NOT NULL AND finished_at_ms IS NOT NULL)
    )
);

CREATE INDEX IF NOT EXISTS idx_launch_planning_state_updated
    ON launch_planning(state, updated_at_ms);

CREATE INDEX IF NOT EXISTS idx_launch_planning_generation_state
    ON launch_planning(daemon_generation_id, state);

CREATE TABLE IF NOT EXISTS worker_process (
    worker_instance_id TEXT PRIMARY KEY,
    boot_identity_hash TEXT NOT NULL,
    session_capsule_hash TEXT NOT NULL,
    boot_epoch INTEGER NOT NULL CHECK (boot_epoch > 0),
    lifecycle_generation INTEGER NOT NULL CHECK (lifecycle_generation > 0),
    process_identity TEXT NOT NULL,
    control_channel_identity TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('starting', 'attached', 'live', 'draining', 'dead')),
    daemon_generation_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    cleanup_state TEXT NOT NULL CHECK (cleanup_state IN ('owned', 'draining', 'reaped', 'unproved')),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_worker_process_daemon_state
    ON worker_process(daemon_generation_id, state);

CREATE UNIQUE INDEX IF NOT EXISTS idx_worker_process_session_epoch
    ON worker_process(session_id, boot_epoch);

CREATE TABLE IF NOT EXISTS dedicated_session (
    session_id TEXT PRIMARY KEY,
    root_thread_id TEXT NOT NULL UNIQUE,
    owner_principal TEXT NOT NULL,
    admitted_capsule_hash TEXT NOT NULL,
    worker_instance_id TEXT,
    worker_boot_epoch INTEGER,
    workspace_id TEXT NOT NULL,
    candidate_required INTEGER NOT NULL CHECK (candidate_required IN (0, 1)),
    credential_profile_id TEXT NOT NULL,
    credential_generation INTEGER NOT NULL CHECK (credential_generation > 0),
    remote_thread_id TEXT,
    current_turn_id TEXT,
    state TEXT NOT NULL CHECK (state IN ('admitted', 'binding', 'idle', 'turn_running', 'awaiting_approval', 'recovering', 'outcome_unknown', 'draining', 'freezing', 'frozen', 'verifying', 'publish_ready', 'publishing', 'discarding', 'terminal')),
    send_boundary TEXT NOT NULL CHECK (send_boundary IN ('none', 'committed', 'contacted', 'settled', 'outcome_unknown')),
    candidate_snapshot_hash TEXT,
    candidate_validation_hash TEXT,
    publication_result TEXT,
    disposition_resume_state TEXT,
    terminal_reason TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_dedicated_session_owner_state
    ON dedicated_session(owner_principal, state);

CREATE TABLE IF NOT EXISTS dedicated_session_command (
    session_id TEXT NOT NULL,
    command_sequence INTEGER NOT NULL CHECK (command_sequence > 0),
    idempotency_key TEXT NOT NULL,
    worker_boot_epoch INTEGER NOT NULL CHECK (worker_boot_epoch > 0),
    command_kind TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('committed', 'dispatched', 'completed', 'failed', 'outcome_unknown')),
    result_json TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (session_id, command_sequence),
    UNIQUE (session_id, idempotency_key)
);

CREATE INDEX IF NOT EXISTS idx_dedicated_session_command_state
    ON dedicated_session_command(session_id, state);

CREATE TABLE IF NOT EXISTS dedicated_session_observation_batch (
    session_id TEXT NOT NULL,
    worker_boot_epoch INTEGER NOT NULL CHECK (worker_boot_epoch > 0),
    first_sequence INTEGER NOT NULL CHECK (first_sequence > 0),
    through_sequence INTEGER NOT NULL CHECK (through_sequence >= first_sequence),
    previous_digest TEXT,
    batch_digest TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('append_contacting', 'settled', 'append_unknown')),
    created_at_ms INTEGER NOT NULL,
    settled_at_ms INTEGER,
    PRIMARY KEY (session_id, worker_boot_epoch, first_sequence)
);

CREATE TABLE IF NOT EXISTS dedicated_session_approval (
    session_id TEXT NOT NULL,
    approval_id TEXT NOT NULL,
    worker_instance_id TEXT NOT NULL,
    worker_boot_epoch INTEGER NOT NULL CHECK (worker_boot_epoch > 0),
    request_digest TEXT NOT NULL,
    operation_class TEXT NOT NULL,
    requested_authority_json TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'decision_reserved', 'delivery_contacting', 'delivery_settled', 'delivery_unknown', 'expired', 'stale_epoch')),
    decision_principal TEXT,
    decision_json TEXT,
    decision_digest TEXT,
    reservation_token TEXT,
    expires_at_ms INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    resolved_at_ms INTEGER,
    delivery_contacted_at_ms INTEGER,
    delivery_settled_at_ms INTEGER,
    PRIMARY KEY (session_id, approval_id)
);

CREATE INDEX IF NOT EXISTS idx_dedicated_session_approval_pending
    ON dedicated_session_approval(session_id, state, expires_at_ms);

CREATE TABLE IF NOT EXISTS credential_profile (
    profile_id TEXT PRIMARY KEY,
    owner_principal TEXT NOT NULL,
    home_id TEXT NOT NULL UNIQUE,
    credential_generation INTEGER NOT NULL CHECK (credential_generation > 0),
    state TEXT NOT NULL CHECK (state IN ('unauthenticated', 'enrolling', 'confirming', 'active', 'revoking', 'revoked', 'deleting')),
    active_login_id TEXT,
    login_epoch INTEGER NOT NULL CHECK (login_epoch >= 0),
    login_expires_at_ms INTEGER,
    sanitized_account_json TEXT,
    lock_owner TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_credential_profile_owner_state
    ON credential_profile(owner_principal, state);
"#;

use ryeos_state::sqlite_schema;

/// The existing pre-epoch runtime application ID. It is recognized only as
/// owned predecessor authority for an explicitly confirmed clean cutover.
const PREDECESSOR_RUNTIME_APP_ID: i32 = 0x5259_4541;

/// Monotonic operator-owned runtime store contract. The low 8 bits of the
/// SQLite application ID carry this epoch; the high 24 bits reserve a
/// runtime-only family that cannot overlap RyeOS's other `RY..` database IDs.
/// A future epoch therefore remains identifiable without decoding or rewriting
/// any predecessor rows.
const RUNTIME_OPERATOR_APP_ID_PREFIX: u32 = 0x5259_0000;
const RUNTIME_OPERATOR_APP_ID_MASK: u32 = 0xffff_ff00;
const RUNTIME_OPERATOR_SCHEMA_EPOCH_MASK: u32 = 0x0000_00ff;
// Epoch 3 is the clean effective-program/hook-dispatch activation barrier. An
// older store may contain resumable occurrences under superseded identity and
// is deliberately refused by ordinary open; the explicit runtime-history
// reset is permitted only after admission has stopped and resumable work has
// been drained/terminalized.
// Epoch 5 stores only the daemon-projected action result in follow waiter
// terminal envelopes. Predecessor rows retained complete runtime results and
// cannot be reinterpreted as the compact parent-resume contract.
// Epoch 6 atomically introduces durable exclusive-process ownership, its
// dedicated session command/approval ledgers, and opaque credential-profile
// metadata. It deliberately has no open-time migration.
// Epoch 7 adds the durable pre-contact boundary for worker-pushed observation
// batches. It deliberately has no open-time migration.
// Epoch 8 retains every worker boot as immutable process-history evidence and
// uniquely identifies an epoch within its session. Predecessor epoch 7's
// single-row-per-session constraint cannot represent a recovered worker.
// Epoch 9 embeds the exact retained-current-HEAD destination in every durable
// execution-project authority envelope. A predecessor row cannot authorize
// publication under this contract.
const RUNTIME_OPERATOR_SCHEMA_EPOCH: u32 = 9;
const _: () = assert!(
    RUNTIME_OPERATOR_SCHEMA_EPOCH > 0
        && RUNTIME_OPERATOR_SCHEMA_EPOCH <= RUNTIME_OPERATOR_SCHEMA_EPOCH_MASK
);
const RUNTIME_APP_ID: i32 = (RUNTIME_OPERATOR_APP_ID_PREFIX | RUNTIME_OPERATOR_SCHEMA_EPOCH) as i32;

/// Schema spec for `runtime.sqlite3` — the single source of truth for
/// what tables/columns/indexes this database must contain.
fn runtime_schema_spec() -> sqlite_schema::SchemaSpec {
    sqlite_schema::SchemaSpec {
        application_id: RUNTIME_APP_ID,
        tables: &[
            sqlite_schema::TableSpec {
                name: "thread_runtime",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "thread_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "chain_root_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "pid",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "pgid",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "metadata",
                        col_type: "BLOB",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "launch_metadata",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "resume_attempts",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "process_identity",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "process_dead_observed_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "stop_requested_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "stop_intent",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "in_process_handler_reservation",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "thread_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "phase",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "created_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "updated_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "thread_commands",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "command_id",
                        col_type: "INTEGER",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "command_type",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "status",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "requested_by",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "params",
                        col_type: "BLOB",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "result",
                        col_type: "BLOB",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "created_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "claimed_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "completed_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "hook_dispatch_ledger",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "dispatch_key",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "seed_version",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "chain_root_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "caller_thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "event",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "hook_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "request_hash",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "status",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "response_json",
                        col_type: "BLOB",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "response_hash",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "created_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "completed_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "detached_spawn_intent",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "operation_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "parent_thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "request_hash",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "child_thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "child_project_authority",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "admitted_launch_capsule_hash",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "launch_metadata",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "initial_events",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "created_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "thread_recovery_wait",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "thread_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "reason",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "detail",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "started_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "deadline_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "thread_launch_claim",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "thread_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "claim_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "claimed_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "lease_expires_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "claimed_by",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "thread_launch_epoch",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "thread_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "last_epoch",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "execution_workspace",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "workspace_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "launch_owner",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "backend_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "lower_snapshot",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "frozen_snapshot_hash",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "root_path",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "state",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "process_identity",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "created_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "updated_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "backend_version",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "pinned_root_identities",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "mount_identity",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "follow_waiter",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "follow_key",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "parent_thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "parent_chain_root_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "parent_successor_thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "follow_node",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "graph_run_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "step_count",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "frontier_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "phase",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "created_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "updated_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "fanout",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "expected_children",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "child_project_authority",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "follow_waiter_child",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "follow_key",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "item_index",
                        col_type: "INTEGER",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "item_ref",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "spec_hash",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "child_thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "child_chain_root_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "sealed_root_request",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "terminal_thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "terminal_status",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "terminal_envelope",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "created_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "updated_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "thread_child_link",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "child_thread_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "parent_thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "relation",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "created_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "launch_window",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "child_chain_root_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "window_key",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "width",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "created_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "launched_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "cancelled_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "seat_lease",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "seat_thread_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "owner",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "surface",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "client_ref",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "last_seen_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "reaping_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "launch_planning",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "launch_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "reserved_thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "requested_by",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "daemon_generation_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "state",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "bound_thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "outcome_code",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "created_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "updated_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "finished_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "worker_process",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "worker_instance_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "boot_identity_hash",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "session_capsule_hash",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "boot_epoch",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "lifecycle_generation",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "process_identity",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "control_channel_identity",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "state",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "daemon_generation_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "session_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "cleanup_state",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "created_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "updated_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "dedicated_session",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "session_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "root_thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "owner_principal",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "admitted_capsule_hash",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "worker_instance_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "worker_boot_epoch",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "workspace_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "candidate_required",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "credential_profile_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "credential_generation",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "remote_thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "current_turn_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "state",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "send_boundary",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "candidate_snapshot_hash",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "candidate_validation_hash",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "publication_result",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "disposition_resume_state",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "terminal_reason",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "created_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "updated_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "dedicated_session_command",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "session_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "command_sequence",
                        col_type: "INTEGER",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "idempotency_key",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "worker_boot_epoch",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "command_kind",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "request_digest",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "payload_json",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "state",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "result_json",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "created_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "updated_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "dedicated_session_approval",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "session_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "approval_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "worker_instance_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "worker_boot_epoch",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "request_digest",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "operation_class",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "requested_authority_json",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "state",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "decision_principal",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "decision_json",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "decision_digest",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "reservation_token",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "expires_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "created_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "resolved_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "delivery_contacted_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "delivery_settled_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "dedicated_session_observation_batch",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "session_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "worker_boot_epoch",
                        col_type: "INTEGER",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "first_sequence",
                        col_type: "INTEGER",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "through_sequence",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "previous_digest",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "batch_digest",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "state",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "created_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "settled_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "credential_profile",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "profile_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "owner_principal",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "home_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "credential_generation",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "state",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "active_login_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "login_epoch",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "login_expires_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "sanitized_account_json",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "lock_owner",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "created_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "updated_at_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
        ],
        indexes: &[
            sqlite_schema::IndexSpec {
                name: "idx_thread_runtime_chain_root",
                table: "thread_runtime",
                columns: &["chain_root_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_in_process_handler_reservation_phase_thread",
                table: "in_process_handler_reservation",
                columns: &["phase", "thread_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_execution_workspace_thread",
                table: "execution_workspace",
                columns: &["thread_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_thread_commands_thread_status",
                table: "thread_commands",
                columns: &["thread_id", "status"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_hook_dispatch_ledger_chain_root",
                table: "hook_dispatch_ledger",
                columns: &["chain_root_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_follow_waiter_successor",
                table: "follow_waiter",
                columns: &["parent_successor_thread_id"],
                unique: true,
            },
            sqlite_schema::IndexSpec {
                name: "idx_follow_waiter_child_chain2",
                table: "follow_waiter_child",
                columns: &["child_chain_root_id"],
                unique: true,
            },
            sqlite_schema::IndexSpec {
                name: "idx_thread_child_link_parent",
                table: "thread_child_link",
                columns: &["parent_thread_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_launch_window_key",
                table: "launch_window",
                columns: &["window_key"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_seat_lease_last_seen",
                table: "seat_lease",
                columns: &["last_seen_at_ms"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_launch_planning_state_updated",
                table: "launch_planning",
                columns: &["state", "updated_at_ms"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_launch_planning_generation_state",
                table: "launch_planning",
                columns: &["daemon_generation_id", "state"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_worker_process_daemon_state",
                table: "worker_process",
                columns: &["daemon_generation_id", "state"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_worker_process_session_epoch",
                table: "worker_process",
                columns: &["session_id", "boot_epoch"],
                unique: true,
            },
            sqlite_schema::IndexSpec {
                name: "idx_dedicated_session_owner_state",
                table: "dedicated_session",
                columns: &["owner_principal", "state"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_dedicated_session_command_state",
                table: "dedicated_session_command",
                columns: &["session_id", "state"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_dedicated_session_approval_pending",
                table: "dedicated_session_approval",
                columns: &["session_id", "state", "expires_at_ms"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_credential_profile_owner_state",
                table: "credential_profile",
                columns: &["owner_principal", "state"],
                unique: false,
            },
        ],
    }
}

fn runtime_user_tables(conn: &Connection) -> Result<BTreeSet<String>> {
    let mut statement = conn.prepare(
        "SELECT name FROM sqlite_master
         WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let tables = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    Ok(tables)
}

const PROJECT_AUTHORITY_ENVELOPE_KIND: &str = "execution_project_authority";
// Epoch 3 adds the exact principal/project/base destination for explicit
// retained-current-HEAD publication. There is deliberately no compatibility
// reader: a predecessor authority cannot be upgraded into publication rights.
const PROJECT_AUTHORITY_SCHEMA_EPOCH: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
struct IncompatibleRuntimeExecutionSchema {
    reason: String,
    predecessor: bool,
}

impl std::fmt::Display for IncompatibleRuntimeExecutionSchema {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl std::error::Error for IncompatibleRuntimeExecutionSchema {}

fn incompatible_runtime_execution_schema(reason: String, predecessor: bool) -> anyhow::Error {
    let guidance = predecessor.then(|| {
        format!(
            "{reason}; stop the daemon and run `{}` before restarting",
            crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND
        )
    });
    let error = anyhow::Error::new(IncompatibleRuntimeExecutionSchema {
        reason,
        predecessor,
    });
    match guidance {
        Some(guidance) => error.context(guidance),
        None => error,
    }
}

fn requires_execution_schema_cutover(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<IncompatibleRuntimeExecutionSchema>()
            .is_some_and(|mismatch| mismatch.predecessor)
            || cause
                .downcast_ref::<IncompatibleRuntimeOperatorSchema>()
                .is_some_and(IncompatibleRuntimeOperatorSchema::is_predecessor)
    })
}

fn is_newer_execution_schema(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<IncompatibleRuntimeExecutionSchema>()
            .is_some_and(|mismatch| !mismatch.predecessor)
            || cause
                .downcast_ref::<IncompatibleRuntimeOperatorSchema>()
                .is_some_and(|mismatch| !mismatch.is_predecessor())
    })
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedProjectAuthorityEnvelope {
    kind: String,
    schema_epoch: u32,
    authority: ryeos_state::objects::ExecutionProjectAuthority,
}

fn decode_current_project_authority(
    raw: &str,
) -> Result<ryeos_state::objects::ExecutionProjectAuthority> {
    let value: Value = serde_json::from_str(raw).context("decode stored project authority")?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("stored project authority envelope must be an object"))?;
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("stored project authority envelope has no string kind"))?;
    if kind != PROJECT_AUTHORITY_ENVELOPE_KIND {
        let reason = format!(
            "stored project authority is not the exact current contract: stored kind={kind:?}, current kind={PROJECT_AUTHORITY_ENVELOPE_KIND:?}"
        );
        if matches!(kind, "projectless" | "live_project" | "pinned_generation") {
            return Err(incompatible_runtime_execution_schema(reason, true));
        }
        bail!("{reason}");
    }
    let schema_epoch = object
        .get("schema_epoch")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("stored project authority envelope has no numeric schema_epoch"))?;
    if schema_epoch != u64::from(PROJECT_AUTHORITY_SCHEMA_EPOCH) {
        return Err(incompatible_runtime_execution_schema(
            format!(
                "stored project authority is not the exact current contract: stored schema_epoch={schema_epoch}, current schema_epoch={PROJECT_AUTHORITY_SCHEMA_EPOCH}"
            ),
            schema_epoch < u64::from(PROJECT_AUTHORITY_SCHEMA_EPOCH),
        ));
    }
    let decoded: PersistedProjectAuthorityEnvelope =
        serde_json::from_value(value.clone()).context("validate current project authority")?;
    decoded.authority.validate()?;
    let canonical =
        lillux::canonical_json(&value).context("canonicalize current project authority")?;
    if canonical != raw {
        bail!("stored project authority is not canonical under the exact current contract");
    }
    Ok(decoded.authority)
}

fn decode_current_project_authority_column(
    column: usize,
    raw: &str,
) -> rusqlite::Result<ryeos_state::objects::ExecutionProjectAuthority> {
    decode_current_project_authority(raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{error:#}"),
            )),
        )
    })
}

fn encode_current_project_authority(
    authority: &ryeos_state::objects::ExecutionProjectAuthority,
) -> Result<String> {
    authority.validate()?;
    let envelope = PersistedProjectAuthorityEnvelope {
        kind: PROJECT_AUTHORITY_ENVELOPE_KIND.to_string(),
        schema_epoch: PROJECT_AUTHORITY_SCHEMA_EPOCH,
        authority: authority.clone(),
    };
    let value = serde_json::to_value(envelope).context("encode current project authority")?;
    lillux::canonical_json(&value).context("canonicalize current project authority")
}

fn decode_current_launch_metadata(raw: &str) -> Result<RuntimeLaunchMetadata> {
    let value: Value = serde_json::from_str(raw).context("decode stored launch metadata")?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("stored launch metadata must be an object"))?;
    let schema_version = object
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("stored launch metadata has no numeric schema_version"))?;
    if schema_version != u64::from(LAUNCH_METADATA_SCHEMA_VERSION) {
        return Err(incompatible_runtime_execution_schema(
            format!(
                "stored launch metadata is not the exact current contract: stored schema_version={schema_version}, current schema_version={LAUNCH_METADATA_SCHEMA_VERSION}"
            ),
            schema_version < u64::from(LAUNCH_METADATA_SCHEMA_VERSION),
        ));
    }
    let decoded: RuntimeLaunchMetadata =
        serde_json::from_value(value.clone()).context("validate current launch metadata")?;
    decoded.validate()?;
    let canonical =
        lillux::canonical_json(&value).context("canonicalize current launch metadata")?;
    if canonical != raw {
        bail!("stored launch metadata is not canonical under the exact current contract");
    }
    Ok(decoded)
}

// The Current variant carries the full launch metadata by design; the
// incompatible arm is a rejection record, so the size skew is inherent.
#[allow(clippy::large_enum_variant)]
enum StoredLaunchMetadata {
    Current(Box<RuntimeLaunchMetadata>),
    Incompatible(IncompatibleLaunchMetadata),
}

impl StoredLaunchMetadata {
    fn current(self) -> Option<RuntimeLaunchMetadata> {
        match self {
            Self::Current(metadata) => Some(*metadata),
            Self::Incompatible(_) => None,
        }
    }

    fn incompatible(self) -> Option<IncompatibleLaunchMetadata> {
        match self {
            Self::Current(_) => None,
            Self::Incompatible(metadata) => Some(metadata),
        }
    }
}

/// Classify predecessor launch authority from outer wire fields only. An
/// unsupported payload is deliberately not deserialized as the current Rust
/// type; it remains opaque history until retention or explicit discard removes
/// it.
fn decode_stored_launch_metadata(raw: &str) -> Result<StoredLaunchMetadata> {
    let value: Value = serde_json::from_str(raw).context("decode stored launch metadata")?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("stored launch metadata must be an object"))?;
    let schema_version = object
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("stored launch metadata has no numeric schema_version"))?;
    let capsule_schema = object
        .get("admitted_launch_capsule_schema")
        .and_then(Value::as_u64);
    let sealed = object
        .get("sealed_root_request")
        .is_some_and(|value| !value.is_null());
    let current_capsule_schema =
        u64::from(ryeos_state::objects::ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION);
    if schema_version != u64::from(LAUNCH_METADATA_SCHEMA_VERSION)
        || (sealed && capsule_schema != Some(current_capsule_schema))
    {
        return Ok(StoredLaunchMetadata::Incompatible(
            IncompatibleLaunchMetadata {
                schema_version,
                admitted_launch_capsule_schema: capsule_schema,
            },
        ));
    }
    decode_current_launch_metadata(raw)
        .map(Box::new)
        .map(StoredLaunchMetadata::Current)
}

fn decode_stored_launch_metadata_column(
    column: usize,
    raw: &str,
) -> rusqlite::Result<StoredLaunchMetadata> {
    decode_stored_launch_metadata(raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{error:#}"),
            )),
        )
    })
}

fn encode_current_launch_metadata(metadata: &RuntimeLaunchMetadata) -> Result<String> {
    metadata.validate()?;
    let value = serde_json::to_value(metadata).context("encode current launch metadata")?;
    lillux::canonical_json(&value).context("canonicalize current launch metadata")
}

fn optional_runtime_text_rows(
    conn: &Connection,
    table: &'static str,
    identity_column: &'static str,
    value_column: &'static str,
) -> Result<Vec<(String, String)>> {
    let object_type = conn
        .query_row(
            "SELECT type FROM sqlite_master WHERE name=?1",
            [table],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .with_context(|| format!("inspect runtime schema object `{table}`"))?;
    let Some(object_type) = object_type else {
        return Ok(Vec::new());
    };
    if object_type != "table" {
        bail!(
            "runtime schema object `{table}` is {object_type:?}, not a table; refusing destructive reset"
        );
    }

    let columns = {
        let mut statement = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .with_context(|| format!("inspect runtime table `{table}`"))?;
        statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<BTreeSet<_>>>()?
    };
    if !columns.contains(value_column) {
        return Ok(Vec::new());
    }
    if !columns.contains(identity_column) {
        bail!(
            "runtime table `{table}` contains `{value_column}` without `{identity_column}`; refusing destructive reset"
        );
    }

    let mut statement = conn
        .prepare(&format!(
            "SELECT {identity_column}, {value_column}
               FROM {table}
              WHERE {value_column} IS NOT NULL
              ORDER BY {identity_column}"
        ))
        .with_context(|| format!("inspect runtime authority rows in `{table}`"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .with_context(|| format!("read runtime authority rows from `{table}`"))?;
    Ok(rows)
}

fn reject_newer_runtime_execution_epochs(conn: &Connection) -> Result<()> {
    let mut launch_rows = Vec::new();
    launch_rows.extend(
        optional_runtime_text_rows(conn, "thread_runtime", "thread_id", "launch_metadata")?
            .into_iter()
            .map(|(owner_id, raw)| ("thread_runtime", owner_id, raw)),
    );
    launch_rows.extend(
        optional_runtime_text_rows(
            conn,
            "detached_spawn_intent",
            "operation_id",
            "launch_metadata",
        )?
        .into_iter()
        .map(|(owner_id, raw)| ("detached_spawn_intent", owner_id, raw)),
    );
    launch_rows.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
    for (owner, owner_id, raw) in launch_rows {
        let value: Value = serde_json::from_str(&raw)
            .with_context(|| format!("inspect launch schema for {owner} row `{owner_id}`"))?;
        let object = value.as_object().ok_or_else(|| {
            anyhow!("launch metadata for {owner} row `{owner_id}` must be an object")
        })?;
        let schema_version = object
            .get("schema_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                anyhow!(
                    "launch metadata for {owner} row `{owner_id}` has no numeric schema_version"
                )
            })?;
        if schema_version > u64::from(LAUNCH_METADATA_SCHEMA_VERSION) {
            return Err(incompatible_runtime_execution_schema(
                format!(
                    "launch metadata for {owner} row `{owner_id}` is newer than the exact current contract: stored schema_version={schema_version}, current schema_version={LAUNCH_METADATA_SCHEMA_VERSION}"
                ),
                false,
            ));
        }
        if let Some(capsule_schema) = object
            .get("admitted_launch_capsule_schema")
            .and_then(Value::as_u64)
        {
            let current = u64::from(ryeos_state::objects::ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION);
            if capsule_schema > current {
                return Err(incompatible_runtime_execution_schema(
                    format!(
                        "launch metadata for {owner} row `{owner_id}` carries a newer admitted launch capsule: stored schema={capsule_schema}, current schema={current}"
                    ),
                    false,
                ));
            }
        }
    }

    let mut authority_rows = Vec::new();
    authority_rows.extend(
        optional_runtime_text_rows(
            conn,
            "detached_spawn_intent",
            "operation_id",
            "child_project_authority",
        )?
        .into_iter()
        .map(|(owner_id, raw)| ("detached_spawn_intent", owner_id, raw)),
    );
    authority_rows.extend(
        optional_runtime_text_rows(
            conn,
            "follow_waiter",
            "follow_key",
            "child_project_authority",
        )?
        .into_iter()
        .map(|(owner_id, raw)| ("follow_waiter", owner_id, raw)),
    );
    authority_rows.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
    for (owner, owner_id, raw) in authority_rows {
        let value: Value = serde_json::from_str(&raw)
            .with_context(|| format!("inspect project authority for {owner} row `{owner_id}`"))?;
        let object = value.as_object().ok_or_else(|| {
            anyhow!("project authority for {owner} row `{owner_id}` must be an object")
        })?;
        let kind = object.get("kind").and_then(Value::as_str).ok_or_else(|| {
            anyhow!("project authority for {owner} row `{owner_id}` has no string kind")
        })?;
        if matches!(kind, "projectless" | "live_project" | "pinned_generation") {
            continue;
        }
        if kind != PROJECT_AUTHORITY_ENVELOPE_KIND {
            bail!(
                "project authority for {owner} row `{owner_id}` has an unrecognized outer kind {kind:?}; refusing destructive reset"
            );
        }
        let schema_epoch = object
            .get("schema_epoch")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                anyhow!(
                    "project authority for {owner} row `{owner_id}` has no numeric schema_epoch"
                )
            })?;
        if schema_epoch > u64::from(PROJECT_AUTHORITY_SCHEMA_EPOCH) {
            return Err(incompatible_runtime_execution_schema(
                format!(
                    "project authority for {owner} row `{owner_id}` is newer than the exact current contract: stored schema_epoch={schema_epoch}, current schema_epoch={PROJECT_AUTHORITY_SCHEMA_EPOCH}"
                ),
                false,
            ));
        }
    }
    Ok(())
}

fn initialize_current_runtime_schema(conn: &Connection, path: &Path) -> Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .context("begin atomic runtime schema initialization")?;
    sqlite_schema::init_owned(&tx, &runtime_schema_spec(), SCHEMA_SQL, path)?;
    assert_current_runtime_schema(&tx, path)?;
    tx.commit()
        .context("commit atomic runtime schema initialization")
}

fn validate_current_runtime_store(conn: &Connection, path: &Path) -> Result<()> {
    let tx = conn
        .unchecked_transaction()
        .context("begin atomic runtime schema/data validation")?;
    let stored_epoch = runtime_operator_schema_epoch(&tx, path)?;
    if stored_epoch > RUNTIME_OPERATOR_SCHEMA_EPOCH {
        return Err(incompatible_runtime_operator_schema(stored_epoch));
    }
    // A predecessor store becomes destructive-reset eligible only after every
    // recognized authority-bearing column has been checked for a future epoch.
    // Missing predecessor tables/columns prove absence at that known location;
    // malformed or unrecognized authority fails closed.
    reject_newer_runtime_execution_epochs(&tx)?;
    if stored_epoch != RUNTIME_OPERATOR_SCHEMA_EPOCH {
        return Err(incompatible_runtime_operator_schema(stored_epoch));
    }
    assert_current_runtime_schema(&tx, path)?;
    let rows = {
        let mut statement = tx.prepare(
            "SELECT 'thread_runtime', thread_id, launch_metadata
               FROM thread_runtime WHERE launch_metadata IS NOT NULL
             UNION ALL
             SELECT 'detached_spawn_intent', operation_id, launch_metadata
               FROM detached_spawn_intent WHERE launch_metadata IS NOT NULL",
        )?;
        // Preserve statement/query temporary drop order under Edition 2024.
        #[allow(clippy::let_and_return)]
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for (owner, owner_id, raw) in rows {
        decode_stored_launch_metadata(&raw)
            .with_context(|| format!("validate launch metadata for {owner} row `{owner_id}`"))?;
    }
    let reservation_count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM in_process_handler_reservation",
        [],
        |row| row.get(0),
    )?;
    if usize::try_from(reservation_count)
        .context("in-process handler reservation count is invalid")?
        > MAX_IN_PROCESS_HANDLER_RESERVATIONS
    {
        bail!(
            "in-process handler reservations exceed the current-schema limit of {MAX_IN_PROCESS_HANDLER_RESERVATIONS}"
        );
    }
    let reservations = {
        let mut statement = tx.prepare(
            "SELECT reservation.thread_id, reservation.phase, runtime.launch_metadata
               FROM in_process_handler_reservation AS reservation
               LEFT JOIN thread_runtime AS runtime
                 ON runtime.thread_id = reservation.thread_id
              ORDER BY reservation.thread_id",
        )?;
        // Preserve statement/query temporary drop order under Edition 2024.
        #[allow(clippy::let_and_return)]
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for (thread_id, phase, raw) in reservations {
        InProcessHandlerReservationPhase::parse(&phase)
            .with_context(|| format!("validate reservation for thread `{thread_id}`"))?;
        let raw = raw.ok_or_else(|| {
            anyhow!("in-process handler reservation `{thread_id}` has no runtime launch metadata")
        })?;
        let metadata = decode_current_launch_metadata(&raw).with_context(|| {
            format!("validate reserved in-process launch metadata for thread `{thread_id}`")
        })?;
        if metadata.launch_driver
            != Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
        {
            bail!("in-process handler reservation `{thread_id}` has a different launch driver");
        }
    }
    let authorities = {
        let mut statement = tx.prepare(
            "SELECT 'detached_spawn_intent', operation_id, child_project_authority
               FROM detached_spawn_intent WHERE child_project_authority IS NOT NULL
             UNION ALL
             SELECT 'follow_waiter', follow_key, child_project_authority
               FROM follow_waiter WHERE child_project_authority IS NOT NULL",
        )?;
        // Preserve statement/query temporary drop order under Edition 2024.
        #[allow(clippy::let_and_return)]
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for (owner, owner_id, raw) in authorities {
        decode_current_project_authority(&raw)
            .with_context(|| format!("validate project authority for {owner} row `{owner_id}`"))?;
    }
    tx.commit()
        .context("finish atomic runtime schema/data validation")
}

/// Destructively replace an owned predecessor runtime schema with the exact
/// current empty schema. This is not an open-time migration: the only caller is
/// the explicitly confirmed offline all-thread-history reset. No predecessor
/// row or continuation fact is interpreted or carried forward.
fn reset_owned_runtime_schema(conn: &Connection, path: &Path) -> Result<()> {
    let operator_epoch = runtime_operator_schema_epoch(conn, path)?;
    if operator_epoch >= RUNTIME_OPERATOR_SCHEMA_EPOCH {
        bail!(
            "runtime database operator schema epoch is {operator_epoch}, expected a proven predecessor of {RUNTIME_OPERATOR_SCHEMA_EPOCH}; refusing explicit reset of {}",
            path.display()
        );
    }
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .context("verify runtime database integrity before explicit reset")?;
    if integrity != "ok" {
        bail!(
            "runtime database integrity check failed before explicit reset for {}: {integrity}",
            path.display()
        );
    }

    let spec = runtime_schema_spec();
    let expected_tables = spec
        .tables
        .iter()
        .map(|table| table.name.to_string())
        .collect::<BTreeSet<_>>();
    let actual_tables = runtime_user_tables(conn)?;
    let unexpected_tables = actual_tables
        .difference(&expected_tables)
        .cloned()
        .collect::<BTreeSet<_>>();
    if !unexpected_tables.is_empty() {
        bail!(
            "owned runtime database contains unexpected tables {:?}; refusing destructive reset of {}",
            unexpected_tables,
            path.display()
        );
    }

    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .context("begin atomic explicit runtime reset")?;
    for table in actual_tables.iter().rev() {
        tx.execute_batch(&format!("DROP TABLE \"{table}\";"))
            .with_context(|| format!("drop owned runtime table `{table}` during explicit reset"))?;
    }
    sqlite_schema::init_owned(&tx, &spec, SCHEMA_SQL, path)?;
    tx.execute("DELETE FROM sqlite_sequence", [])
        .context("clear runtime sequence state during explicit reset")?;
    assert_current_runtime_schema(&tx, path)?;
    tx.commit()
        .context("commit atomic explicit runtime reset")?;
    tracing::warn!(database = %path.display(), "explicitly reset incompatible runtime history without migration");
    Ok(())
}

pub struct RuntimeDb {
    conn: Connection,
    reset_required: bool,
    open_mode: RuntimeDbOpenMode,
    _directory: Option<lillux::PinnedDirectory>,
    _directory_lock: Option<lillux::secure_fs::PinnedDirectoryLock>,
    _database_file: Option<File>,
    _wal_file: Option<File>,
    _shm_file: Option<File>,
    _inspection_copy: Option<crate::temp_dir_guard::TempDirGuard>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerProcessState {
    Starting,
    Attached,
    Live,
    Draining,
    Dead,
}

impl WorkerProcessState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Attached => "attached",
            Self::Live => "live",
            Self::Draining => "draining",
            Self::Dead => "dead",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "starting" => Ok(Self::Starting),
            "attached" => Ok(Self::Attached),
            "live" => Ok(Self::Live),
            "draining" => Ok(Self::Draining),
            "dead" => Ok(Self::Dead),
            other => bail!("invalid worker process state `{other}`"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerProcessRecord {
    pub worker_instance_id: String,
    pub boot_identity_hash: String,
    pub session_capsule_hash: String,
    pub boot_epoch: u64,
    pub lifecycle_generation: u64,
    pub process_identity: ExecutionProcessIdentity,
    pub control_channel_identity: String,
    pub state: WorkerProcessState,
    pub daemon_generation_id: String,
    pub session_id: String,
    pub cleanup_state: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDedicatedSession<'a> {
    pub session_id: &'a str,
    pub root_thread_id: &'a str,
    pub owner_principal: &'a str,
    pub admitted_capsule_hash: &'a str,
    pub workspace_id: &'a str,
    pub candidate_required: bool,
    pub credential_profile_id: &'a str,
    pub credential_generation: u64,
    pub credential_lock_owner: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DedicatedSessionRecord {
    pub session_id: String,
    pub root_thread_id: String,
    pub owner_principal: String,
    pub admitted_capsule_hash: String,
    pub worker_instance_id: Option<String>,
    pub worker_boot_epoch: Option<u64>,
    pub workspace_id: String,
    pub candidate_required: bool,
    pub credential_profile_id: String,
    pub credential_generation: u64,
    pub remote_thread_id: Option<String>,
    pub current_turn_id: Option<String>,
    pub state: String,
    pub send_boundary: String,
    pub candidate_snapshot_hash: Option<String>,
    pub candidate_validation_hash: Option<String>,
    pub publication_result: Option<String>,
    pub terminal_reason: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCredentialProfile<'a> {
    pub profile_id: &'a str,
    pub owner_principal: &'a str,
    pub home_id: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialProfileRecord {
    pub profile_id: String,
    pub owner_principal: String,
    pub home_id: String,
    pub credential_generation: u64,
    pub state: String,
    pub active_login_id: Option<String>,
    pub login_epoch: u64,
    pub login_expires_at_ms: Option<i64>,
    pub sanitized_account: Option<serde_json::Value>,
    pub lock_owner: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDedicatedSessionCommand<'a> {
    pub session_id: &'a str,
    pub idempotency_key: &'a str,
    pub worker_boot_epoch: u64,
    pub command_kind: &'a str,
    pub request_digest: &'a str,
    pub payload: &'a serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DedicatedSessionCommandRecord {
    pub session_id: String,
    pub command_sequence: u64,
    pub idempotency_key: String,
    pub worker_boot_epoch: u64,
    pub command_kind: String,
    pub request_digest: String,
    pub payload: serde_json::Value,
    pub state: String,
    pub result: Option<serde_json::Value>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationBatchReservation {
    ContactAppend,
    AlreadySettled,
    RebuildProjection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedicatedObservationBatchRecord {
    pub session_id: String,
    pub worker_boot_epoch: u64,
    pub first_sequence: u64,
    pub through_sequence: u64,
    pub batch_digest: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDedicatedSessionApproval<'a> {
    pub session_id: &'a str,
    pub approval_id: &'a str,
    pub worker_instance_id: &'a str,
    pub worker_boot_epoch: u64,
    pub request_digest: &'a str,
    pub operation_class: &'a str,
    pub requested_authority: &'a serde_json::Value,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DedicatedSessionApprovalRecord {
    pub session_id: String,
    pub approval_id: String,
    pub worker_instance_id: String,
    pub worker_boot_epoch: u64,
    pub request_digest: String,
    pub operation_class: String,
    pub requested_authority: serde_json::Value,
    pub state: String,
    pub decision_principal: Option<String>,
    pub decision: Option<serde_json::Value>,
    pub decision_digest: Option<String>,
    pub reservation_token: Option<String>,
    pub expires_at_ms: i64,
    pub created_at_ms: i64,
    pub resolved_at_ms: Option<i64>,
    pub delivery_contacted_at_ms: Option<i64>,
    pub delivery_settled_at_ms: Option<i64>,
}

fn validate_worker_process_record(record: &WorkerProcessRecord) -> Result<()> {
    for (label, value, max) in [
        (
            "worker instance id",
            record.worker_instance_id.as_str(),
            256,
        ),
        (
            "worker boot identity",
            record.boot_identity_hash.as_str(),
            128,
        ),
        (
            "worker session capsule",
            record.session_capsule_hash.as_str(),
            128,
        ),
        (
            "worker control channel",
            record.control_channel_identity.as_str(),
            1024,
        ),
        (
            "worker daemon generation",
            record.daemon_generation_id.as_str(),
            256,
        ),
        ("worker session id", record.session_id.as_str(), 256),
        ("worker cleanup state", record.cleanup_state.as_str(), 32),
    ] {
        validate_bounded_runtime_text(label, value, max)?;
    }
    validate_execution_process_identity_shape(&record.process_identity)
        .context("invalid worker process identity")?;
    if record.boot_epoch == 0
        || record.lifecycle_generation == 0
        || record.created_at_ms <= 0
        || record.updated_at_ms < record.created_at_ms
        || !matches!(
            record.cleanup_state.as_str(),
            "owned" | "draining" | "reaped" | "unproved"
        )
    {
        bail!("worker process record is internally inconsistent");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IncompatibleRuntimeOperatorSchema {
    stored: u32,
    current: u32,
}

impl IncompatibleRuntimeOperatorSchema {
    fn is_predecessor(&self) -> bool {
        self.stored < self.current
    }
}

impl std::fmt::Display for IncompatibleRuntimeOperatorSchema {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "runtime database is not the exact current operator contract: stored schema_epoch={}, current schema_epoch={}",
            self.stored, self.current
        )
    }
}

impl std::error::Error for IncompatibleRuntimeOperatorSchema {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeDbOpenMode {
    Current,
    ExistingCurrent,
    ExplicitHistoryReset,
    ExplicitHistoryResetInspection,
    ExplicitHistoryResetInspectionCopy,
}

impl RuntimeDbOpenMode {
    fn allow_create(self) -> bool {
        matches!(self, Self::Current | Self::ExplicitHistoryReset)
    }

    fn explicit_history_reset(self) -> bool {
        matches!(
            self,
            Self::ExplicitHistoryReset
                | Self::ExplicitHistoryResetInspection
                | Self::ExplicitHistoryResetInspectionCopy
        )
    }

    fn materializes_inspection_copy(self) -> bool {
        matches!(self, Self::ExplicitHistoryResetInspection)
    }
}

fn runtime_operator_schema_epoch(conn: &Connection, path: &Path) -> Result<u32> {
    let application_id: i32 = conn
        .query_row("PRAGMA application_id", [], |row| row.get(0))
        .context("read runtime database application_id")?;
    if application_id == PREDECESSOR_RUNTIME_APP_ID {
        return Ok(0);
    }
    let encoded = u32::try_from(application_id).map_err(|_| {
        anyhow!(
            "runtime database application_id is {application_id}; refusing to classify or reset unowned store {}",
            path.display()
        )
    })?;
    if encoded & RUNTIME_OPERATOR_APP_ID_MASK != RUNTIME_OPERATOR_APP_ID_PREFIX {
        bail!(
            "runtime database application_id is {application_id}; refusing to classify or reset unowned store {}",
            path.display()
        );
    }
    let epoch = encoded & RUNTIME_OPERATOR_SCHEMA_EPOCH_MASK;
    if epoch == 0 {
        bail!(
            "runtime database has an invalid zero operator schema epoch; refusing to classify or reset {}",
            path.display()
        );
    }
    Ok(epoch)
}

fn incompatible_runtime_operator_schema(stored: u32) -> anyhow::Error {
    IncompatibleRuntimeOperatorSchema {
        stored,
        current: RUNTIME_OPERATOR_SCHEMA_EPOCH,
    }
    .into()
}

fn assert_current_runtime_schema(conn: &Connection, path: &Path) -> Result<()> {
    let stored_epoch = runtime_operator_schema_epoch(conn, path)?;
    if stored_epoch != RUNTIME_OPERATOR_SCHEMA_EPOCH {
        return Err(incompatible_runtime_operator_schema(stored_epoch));
    }
    sqlite_schema::assert_owned(conn, &runtime_schema_spec(), path)
        .context("runtime database is not the exact current owned schema")?;
    sqlite_schema::assert_complete_schema_sql(conn, SCHEMA_SQL, path)
        .context("runtime database SQL does not match the exact current format")
}

fn runtime_sidecar_name(database_name: &OsStr, suffix: &str) -> OsString {
    let mut name = database_name.to_os_string();
    name.push(suffix);
    name
}

fn inspect_runtime_sidecars(
    directory: &lillux::PinnedDirectory,
    database_name: &OsStr,
) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let name = runtime_sidecar_name(database_name, suffix);
        let _ = directory.open_regular(&name, false).with_context(|| {
            format!(
                "runtime database sidecar must be regular and non-symlink: {}",
                directory.path().join(&name).display()
            )
        })?;
    }
    Ok(())
}

fn copy_sqlite_file_for_inspection(
    source: &lillux::PinnedDirectory,
    destination: &lillux::PinnedDirectory,
    name: &OsStr,
    required: bool,
) -> Result<()> {
    let Some(mut source_file) = source.open_regular(name, false)? else {
        if required {
            bail!(
                "SQLite database is absent: {}",
                source.path().join(name).display()
            );
        }
        return Ok(());
    };
    let expected_len = source_file
        .metadata()
        .with_context(|| format!("inspect SQLite file {}", source.path().join(name).display()))?
        .len();
    let mut destination_file = destination.open_regular_create(name, true, true, 0o600)?;
    let copied = std::io::copy(&mut source_file, &mut destination_file)
        .with_context(|| format!("copy SQLite inspection file {}", name.to_string_lossy()))?;
    if copied != expected_len {
        bail!(
            "SQLite file changed length while being copied for inspection: {}",
            source.path().join(name).display()
        );
    }
    destination_file
        .sync_all()
        .with_context(|| format!("sync SQLite inspection file {}", name.to_string_lossy()))
}

pub(crate) fn create_sqlite_inspection_copy(
    source: &lillux::PinnedDirectory,
    database_name: &OsStr,
    purpose: &'static str,
) -> Result<(lillux::PinnedDirectory, crate::temp_dir_guard::TempDirGuard)> {
    let temp_parent = lillux::PinnedDirectory::open_or_create(&std::env::temp_dir())
        .context("pin system temporary directory for SQLite inspection")?;
    for _ in 0..64 {
        let name = OsString::from(format!(
            "ryeos-{purpose}-inspection-{:032x}",
            rand::random::<u128>()
        ));
        let created = match temp_parent.create_child(&name, 0o700) {
            Ok(created) => created,
            Err(error)
                if error
                    .chain()
                    .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
                    .any(|cause| cause.kind() == std::io::ErrorKind::AlreadyExists) =>
            {
                continue;
            }
            Err(error) => return Err(error).context("create SQLite inspection directory"),
        };
        let working = match created.try_clone() {
            Ok(working) => working,
            Err(error) => {
                let _ = temp_parent.remove_empty_child_if_same(&name, &created);
                return Err(error).context("duplicate SQLite inspection directory authority");
            }
        };
        let guard = crate::temp_dir_guard::TempDirGuard::new_pinned(temp_parent, name, created);
        copy_sqlite_file_for_inspection(source, &working, database_name, true)?;
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = runtime_sidecar_name(database_name, suffix);
            copy_sqlite_file_for_inspection(source, &working, &sidecar, false)?;
        }
        working.sync().context("sync SQLite inspection copy")?;
        return Ok((working, guard));
    }
    bail!("could not allocate a unique SQLite inspection directory")
}

fn ensure_runtime_directory_binding(directory: &lillux::PinnedDirectory) -> Result<()> {
    let current = lillux::PinnedDirectory::open(directory.path())?.ok_or_else(|| {
        anyhow::anyhow!(
            "pinned runtime database directory disappeared: {}",
            directory.path().display()
        )
    })?;
    if !directory.is_same_directory(&current)? {
        bail!(
            "runtime database directory changed while in use: {}",
            directory.path().display()
        );
    }
    Ok(())
}

fn runtime_files_are_same(left: &File, right: &File) -> Result<bool> {
    #[cfg(not(unix))]
    {
        let _ = (left, right);
        bail!("runtime database file identity is unavailable on this platform");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let left = left.metadata()?;
        let right = right.metadata()?;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }
}

fn ensure_runtime_file_binding(
    directory: &lillux::PinnedDirectory,
    name: &OsStr,
    expected: &File,
    label: &str,
) -> Result<()> {
    let current = directory.open_regular(name, false)?.ok_or_else(|| {
        anyhow::anyhow!(
            "{label} disappeared while in use: {}",
            directory.path().join(name).display()
        )
    })?;
    if !runtime_files_are_same(expected, &current)? {
        bail!(
            "{label} changed while in use: {}",
            directory.path().join(name).display()
        );
    }
    Ok(())
}

fn ensure_same_runtime_file(
    expected: &File,
    current: &File,
    label: &str,
    database_path: &Path,
) -> Result<()> {
    if !runtime_files_are_same(expected, current)? {
        bail!(
            "{label} changed while runtime database was opening: {}",
            database_path.display()
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn matching_open_descriptors(file: &File) -> Result<BTreeSet<i32>> {
    use std::os::unix::fs::MetadataExt;

    let expected = file.metadata()?;
    let mut descriptors = BTreeSet::new();
    for entry in fs::read_dir("/proc/self/fd").context("enumerate process descriptors")? {
        let entry = entry.context("read process descriptor entry")?;
        let Some(descriptor) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let metadata = match fs::metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspect process descriptor {}", entry.path().display())
                });
            }
        };
        if metadata.dev() == expected.dev() && metadata.ino() == expected.ino() {
            descriptors.insert(descriptor);
        }
    }
    Ok(descriptors)
}

#[cfg(not(target_os = "linux"))]
fn matching_open_descriptors(_file: &File) -> Result<BTreeSet<i32>> {
    Ok(BTreeSet::new())
}

fn ensure_sqlite_connection_uses_expected_file(
    file: &File,
    descriptors_before: &BTreeSet<i32>,
    label: &str,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let mut descriptors_after = matching_open_descriptors(file)?;
        descriptors_after.remove(&file.as_raw_fd());
        if descriptors_after.is_subset(descriptors_before) {
            bail!("SQLite did not retain a descriptor for the pinned {label} inode");
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (file, descriptors_before, label);
    Ok(())
}

impl ryeos_state::RuntimeLivenessInspector for RuntimeDb {
    fn chain_has_live_recovery_state(&self, chain_root_id: &str) -> Result<bool> {
        self.chain_has_live_state(chain_root_id)
    }
}

fn validate_sha256(field: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("{field} must be a lowercase 64-character hexadecimal SHA-256 digest");
    }
    Ok(())
}

fn validate_new_hook_dispatch(seed: &NewHookDispatch) -> Result<()> {
    if seed.seed_version != HOOK_DISPATCH_SEED_VERSION {
        bail!(
            "hook dispatch seed version {} is not the active version {HOOK_DISPATCH_SEED_VERSION}",
            seed.seed_version
        );
    }
    validate_sha256("dispatch_key", &seed.dispatch_key)?;
    validate_sha256("request_hash", &seed.request_hash)?;
    for (field, value, limit) in [
        ("chain_root_id", seed.chain_root_id.as_str(), 4 * 1024),
        ("caller_thread_id", seed.caller_thread_id.as_str(), 4 * 1024),
        ("event", seed.event.as_str(), 1024),
        ("hook_id", seed.hook_id.as_str(), 4 * 1024),
    ] {
        if value.is_empty() {
            bail!("hook dispatch {field} cannot be empty");
        }
        if value.len() > limit {
            bail!("hook dispatch {field} exceeds {limit} byte limit");
        }
    }
    Ok(())
}

fn validate_detached_spawn_intent(
    operation_id: &str,
    parent_thread_id: &str,
    request_hash: &str,
    child_thread_id: &str,
) -> Result<()> {
    validate_sha256("detached operation_id", operation_id)?;
    validate_sha256("detached request_hash", request_hash)?;
    for (field, value) in [
        ("parent_thread_id", parent_thread_id),
        ("child_thread_id", child_thread_id),
    ] {
        if value.is_empty() {
            bail!("detached spawn {field} cannot be empty");
        }
        if value.len() > 4 * 1024 {
            bail!("detached spawn {field} exceeds 4096 byte limit");
        }
    }
    Ok(())
}

fn validate_detached_spawn_intent_record(intent: &DetachedSpawnIntent) -> Result<()> {
    validate_detached_spawn_intent(
        &intent.operation_id,
        &intent.parent_thread_id,
        &intent.request_hash,
        &intent.child_thread_id,
    )?;
    if let Some(authority) = &intent.child_project_authority {
        authority.validate()?;
    }
    let sealed = intent.launch_metadata.is_some();
    let sealed_fields_complete = intent.admitted_launch_capsule_hash.is_some()
        && intent.launch_metadata.is_some()
        && intent.initial_events.is_some()
        && intent.child_project_authority.is_some();
    let unsealed_fields_empty = intent.admitted_launch_capsule_hash.is_none()
        && intent.launch_metadata.is_none()
        && intent.initial_events.is_none();
    if (sealed && !sealed_fields_complete) || (!sealed && !unsealed_fields_empty) {
        bail!(
            "detached operation `{}` has an incomplete sealed authority",
            intent.operation_id
        );
    }
    if let Some(metadata) = &intent.launch_metadata {
        metadata.validate()?;
        let expected = metadata
            .admitted_launch_capsule()?
            .ok_or_else(|| anyhow!("detached operation has no admitted launch capsule"))?
            .content_hash()?;
        if intent.admitted_launch_capsule_hash.as_deref() != Some(expected.as_str()) {
            bail!(
                "detached operation `{}` admitted capsule hash is not canonical",
                intent.operation_id
            );
        }
    }
    Ok(())
}

fn decode_completed_hook_response(
    dispatch_key: &str,
    response_json: Option<&[u8]>,
    response_hash: Option<&str>,
) -> Result<Value> {
    let response_json = response_json
        .with_context(|| format!("completed hook dispatch `{dispatch_key}` has no response"))?;
    if response_json.len() > MAX_HOOK_DISPATCH_RESPONSE_BYTES {
        bail!("completed hook dispatch `{dispatch_key}` exceeds response size limit");
    }
    let response_hash = response_hash.with_context(|| {
        format!("completed hook dispatch `{dispatch_key}` has no response hash")
    })?;
    validate_sha256("response_hash", response_hash)?;
    let actual_hash = lillux::sha256_hex(response_json);
    if actual_hash != response_hash {
        bail!("completed hook dispatch `{dispatch_key}` response hash mismatch");
    }
    let response: Value = serde_json::from_slice(response_json)
        .with_context(|| format!("completed hook dispatch `{dispatch_key}` has invalid JSON"))?;
    let canonical = lillux::canonical_json(&response)
        .context("canonicalize completed hook dispatch response")?;
    if canonical.as_bytes() != response_json {
        bail!("completed hook dispatch `{dispatch_key}` response is not canonical JSON");
    }
    serde_json::from_value::<ryeos_runtime::callback_contract::CallbackDispatchResponse>(
        response.clone(),
    )
    .with_context(|| {
        format!("completed hook dispatch `{dispatch_key}` violates callback response contract")
    })?;
    Ok(response)
}

#[allow(clippy::too_many_arguments)]
fn decode_completed_hook_dispatch(
    dispatch_key: &str,
    chain_root_id: String,
    occurrence_thread_id: String,
    event: String,
    hook_id: String,
    request_hash: String,
    response_json: Option<&[u8]>,
    response_hash: Option<&str>,
) -> Result<CompletedHookDispatch> {
    let response = decode_completed_hook_response(dispatch_key, response_json, response_hash)?;
    let response_hash = response_hash
        .expect("completed response decoder requires a hash")
        .to_string();
    Ok(CompletedHookDispatch {
        dispatch_key: dispatch_key.to_string(),
        chain_root_id,
        occurrence_thread_id,
        event,
        hook_id,
        request_hash,
        response,
        response_hash,
    })
}

fn read_launch_planning(
    conn: &Connection,
    sql: &str,
    key: &str,
) -> Result<Option<LaunchPlanningRecord>> {
    conn.query_row(sql, params![key], |row| {
        Ok(LaunchPlanningRecord {
            launch_id: row.get(0)?,
            reserved_thread_id: row.get(1)?,
            requested_by: row.get(2)?,
            daemon_generation_id: row.get(3)?,
            state: row.get(4)?,
            bound_thread_id: row.get(5)?,
            outcome_code: row.get(6)?,
        })
    })
    .optional()
    .map_err(Into::into)
}

fn prune_launch_planning(conn: &Connection, now_ms: i64) -> Result<()> {
    const TERMINAL_RETENTION_MS: i64 = 24 * 60 * 60 * 1_000;
    const MAX_TERMINAL_ROWS: i64 = 4_096;
    conn.execute(
        "DELETE FROM launch_planning
          WHERE state IN ('cancelled', 'failed', 'expired')
            AND finished_at_ms < ?1",
        params![now_ms.saturating_sub(TERMINAL_RETENTION_MS)],
    )?;
    conn.execute(
        "DELETE FROM launch_planning
          WHERE launch_id IN (
              SELECT launch_id FROM launch_planning
               WHERE state IN ('cancelled', 'failed', 'expired')
               ORDER BY finished_at_ms DESC, launch_id DESC
               LIMIT -1 OFFSET ?1
          )",
        params![MAX_TERMINAL_ROWS],
    )?;
    Ok(())
}

fn read_dedicated_command_by_key(
    conn: &Connection,
    session_id: &str,
    idempotency_key: &str,
) -> Result<Option<DedicatedSessionCommandRecord>> {
    let row = conn
        .query_row(
            "SELECT session_id, command_sequence, idempotency_key, worker_boot_epoch,
                command_kind, request_digest, payload_json, state, result_json,
                created_at_ms, updated_at_ms
           FROM dedicated_session_command
          WHERE session_id=?1 AND idempotency_key=?2",
            params![session_id, idempotency_key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, i64>(10)?,
                ))
            },
        )
        .optional()?;
    row.map(|row| {
        Ok(DedicatedSessionCommandRecord {
            session_id: row.0,
            command_sequence: u64::try_from(row.1).context("negative command sequence")?,
            idempotency_key: row.2,
            worker_boot_epoch: u64::try_from(row.3).context("negative command worker epoch")?,
            command_kind: row.4,
            request_digest: row.5,
            payload: serde_json::from_str(&row.6).context("decode command payload")?,
            state: row.7,
            result: row
                .8
                .map(|raw| serde_json::from_str(&raw))
                .transpose()
                .context("decode command result")?,
            created_at_ms: row.9,
            updated_at_ms: row.10,
        })
    })
    .transpose()
}

impl RuntimeDb {
    pub fn new_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("failed to open in-memory runtime db")?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .context("enable in-memory runtime database foreign keys")?;
        initialize_current_runtime_schema(&conn, Path::new(":memory:"))?;
        Ok(Self {
            conn,
            reset_required: false,
            open_mode: RuntimeDbOpenMode::Current,
            _directory: None,
            _directory_lock: None,
            _database_file: None,
            _wal_file: None,
            _shm_file: None,
            _inspection_copy: None,
        })
    }

    pub fn admit_dedicated_session(&self, session: NewDedicatedSession<'_>) -> Result<()> {
        for (label, value) in [
            ("dedicated session id", session.session_id),
            ("dedicated root thread id", session.root_thread_id),
            ("dedicated owner principal", session.owner_principal),
            ("dedicated admitted capsule", session.admitted_capsule_hash),
            ("dedicated workspace id", session.workspace_id),
            (
                "dedicated credential profile id",
                session.credential_profile_id,
            ),
        ] {
            validate_bounded_runtime_text(label, value, 256)?;
        }
        if session.credential_generation == 0 {
            bail!("dedicated credential generation must be positive");
        }
        let now = lillux::time::timestamp_millis() as i64;
        let changed = self.conn.execute(
            "INSERT INTO dedicated_session (
                session_id, root_thread_id, owner_principal, admitted_capsule_hash,
                worker_instance_id, worker_boot_epoch, workspace_id, candidate_required,
                credential_profile_id, credential_generation, remote_thread_id,
                current_turn_id, state, send_boundary, candidate_snapshot_hash,
                candidate_validation_hash, publication_result, terminal_reason,
                created_at_ms, updated_at_ms
             ) SELECT ?1, ?2, ?3, ?4, NULL, NULL, ?5, ?6, ?7, ?8, NULL, NULL,
                       'admitted', 'none', NULL, NULL, NULL, NULL, ?10, ?10
               WHERE EXISTS(SELECT 1 FROM credential_profile
                 WHERE profile_id=?7 AND owner_principal=?3 AND credential_generation=?8
                   AND lock_owner=?9 AND state IN ('unauthenticated','enrolling','confirming','active'))",
            params![
                session.session_id,
                session.root_thread_id,
                session.owner_principal,
                session.admitted_capsule_hash,
                session.workspace_id,
                i64::from(session.candidate_required),
                session.credential_profile_id,
                i64::try_from(session.credential_generation)
                    .context("credential generation exceeds SQLite integer range")?,
                session.credential_lock_owner,
                now,
            ],
        )?;
        if changed != 1 {
            bail!("dedicated admission lost its credential generation/lock fence");
        }
        Ok(())
    }

    pub fn dedicated_session(&self, session_id: &str) -> Result<Option<DedicatedSessionRecord>> {
        validate_bounded_runtime_text("dedicated session id", session_id, 256)?;
        let row = self
            .conn
            .query_row(
                "SELECT session_id, root_thread_id, owner_principal, admitted_capsule_hash,
                    worker_instance_id, worker_boot_epoch, workspace_id, candidate_required,
                    credential_profile_id, credential_generation, remote_thread_id,
                    current_turn_id, state, send_boundary, candidate_snapshot_hash,
                    candidate_validation_hash, publication_result, terminal_reason,
                    created_at_ms, updated_at_ms
               FROM dedicated_session WHERE session_id=?1",
                [session_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, i64>(9)?,
                        row.get::<_, Option<String>>(10)?,
                        row.get::<_, Option<String>>(11)?,
                        row.get::<_, String>(12)?,
                        row.get::<_, String>(13)?,
                        row.get::<_, Option<String>>(14)?,
                        row.get::<_, Option<String>>(15)?,
                        row.get::<_, Option<String>>(16)?,
                        row.get::<_, Option<String>>(17)?,
                        row.get::<_, i64>(18)?,
                        row.get::<_, i64>(19)?,
                    ))
                },
            )
            .optional()?;
        row.map(|row| {
            Ok(DedicatedSessionRecord {
                session_id: row.0,
                root_thread_id: row.1,
                owner_principal: row.2,
                admitted_capsule_hash: row.3,
                worker_instance_id: row.4,
                worker_boot_epoch: row
                    .5
                    .map(u64::try_from)
                    .transpose()
                    .context("negative worker boot epoch")?,
                workspace_id: row.6,
                candidate_required: row.7 != 0,
                credential_profile_id: row.8,
                credential_generation: u64::try_from(row.9)
                    .context("negative credential generation")?,
                remote_thread_id: row.10,
                current_turn_id: row.11,
                state: row.12,
                send_boundary: row.13,
                candidate_snapshot_hash: row.14,
                candidate_validation_hash: row.15,
                publication_result: row.16,
                terminal_reason: row.17,
                created_at_ms: row.18,
                updated_at_ms: row.19,
            })
        })
        .transpose()
    }

    pub fn dedicated_sessions_for_credential_profile(
        &self,
        profile_id: &str,
    ) -> Result<Vec<DedicatedSessionRecord>> {
        validate_bounded_runtime_text("credential profile id", profile_id, 256)?;
        let mut statement = self.conn.prepare(
            "SELECT session_id FROM dedicated_session
              WHERE credential_profile_id=?1
              ORDER BY session_id",
        )?;
        let ids = statement
            .query_map([profile_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter()
            .map(|session_id| {
                self.dedicated_session(&session_id)?
                    .ok_or_else(|| anyhow!("listed dedicated session disappeared"))
            })
            .collect()
    }

    pub fn dedicated_sessions_in_state(&self, state: &str) -> Result<Vec<DedicatedSessionRecord>> {
        validate_bounded_runtime_text("dedicated session state", state, 32)?;
        let mut statement = self.conn.prepare(
            "SELECT session_id FROM dedicated_session WHERE state=?1 ORDER BY session_id",
        )?;
        let ids = statement
            .query_map([state], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter()
            .map(|session_id| {
                self.dedicated_session(&session_id)?
                    .ok_or_else(|| anyhow!("listed dedicated session disappeared"))
            })
            .collect()
    }

    pub fn terminalize_unattached_dedicated_session(
        &self,
        session_id: &str,
        reason: &str,
    ) -> Result<()> {
        validate_bounded_runtime_text("dedicated session id", session_id, 256)?;
        validate_bounded_runtime_text("dedicated terminal reason", reason, 2048)?;
        let now = lillux::time::timestamp_millis() as i64;
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE dedicated_session SET state='terminal', terminal_reason=?2,
                    current_turn_id=NULL, send_boundary='none', updated_at_ms=?3
              WHERE session_id=?1 AND worker_instance_id IS NULL AND worker_boot_epoch IS NULL
                AND state IN ('admitted','recovering','outcome_unknown')",
            params![session_id, reason, now],
        )?;
        if changed != 1 {
            bail!("unattached dedicated terminal settlement lost its session CAS");
        }
        tx.execute(
            "UPDATE dedicated_session_approval SET state='delivery_unknown', resolved_at_ms=?2
              WHERE session_id=?1 AND state='delivery_contacting'",
            params![session_id, now],
        )?;
        tx.execute(
            "UPDATE dedicated_session_approval SET state='stale_epoch', resolved_at_ms=?2
              WHERE session_id=?1 AND state IN ('pending', 'decision_reserved')",
            params![session_id, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn fail_dedicated_session_start(
        &self,
        session_id: &str,
        worker_instance_id: &str,
        reason: &str,
        cleanup_proved: bool,
    ) -> Result<()> {
        validate_bounded_runtime_text("dedicated session id", session_id, 256)?;
        validate_bounded_runtime_text("worker instance id", worker_instance_id, 256)?;
        validate_bounded_runtime_text("dedicated terminal reason", reason, 4096)?;
        let now = lillux::time::timestamp_millis() as i64;
        let tx = self.conn.unchecked_transaction()?;
        let next_state = if cleanup_proved {
            "terminal"
        } else {
            "outcome_unknown"
        };
        let changed = tx.execute(
            "UPDATE dedicated_session
                SET state=?5, terminal_reason=?3,
                    send_boundary=CASE WHEN ?5='outcome_unknown' THEN 'outcome_unknown' ELSE send_boundary END,
                    updated_at_ms=?4
              WHERE session_id=?1 AND state IN ('admitted','binding','recovering')",
            params![session_id, worker_instance_id, reason, now, next_state],
        )?;
        if changed != 1 {
            bail!("dedicated start failure lost its session-state CAS");
        }
        if cleanup_proved {
            tx.execute(
                "UPDATE credential_profile SET lock_owner=NULL, updated_at_ms=?3
              WHERE profile_id=(SELECT credential_profile_id FROM dedicated_session
                                  WHERE session_id=?1)
                AND lock_owner=?2
                AND (NOT EXISTS(SELECT 1 FROM worker_process WHERE worker_instance_id=?2)
                     OR EXISTS(SELECT 1 FROM worker_process
                         WHERE worker_instance_id=?2 AND cleanup_state='reaped'))",
                params![session_id, worker_instance_id, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn bind_dedicated_remote_thread(
        &self,
        session_id: &str,
        worker_instance_id: &str,
        worker_boot_epoch: u64,
        remote_thread_id: &str,
    ) -> Result<()> {
        for (label, value) in [
            ("dedicated session id", session_id),
            ("worker instance id", worker_instance_id),
            ("remote thread id", remote_thread_id),
        ] {
            validate_bounded_runtime_text(label, value, 256)?;
        }
        let changed = self.conn.execute(
            "UPDATE dedicated_session SET remote_thread_id=?4, updated_at_ms=?5
              WHERE session_id=?1 AND worker_instance_id=?2 AND worker_boot_epoch=?3
                AND state='idle' AND remote_thread_id IS NULL",
            params![
                session_id,
                worker_instance_id,
                i64::try_from(worker_boot_epoch)
                    .context("worker boot epoch exceeds SQLite range")?,
                remote_thread_id,
                lillux::time::timestamp_millis() as i64
            ],
        )?;
        if changed != 1 {
            let matched: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM dedicated_session
                  WHERE session_id=?1 AND worker_instance_id=?2 AND worker_boot_epoch=?3
                    AND remote_thread_id=?4)",
                params![
                    session_id,
                    worker_instance_id,
                    i64::try_from(worker_boot_epoch)?,
                    remote_thread_id
                ],
                |row| row.get(0),
            )?;
            if !matched {
                bail!("dedicated remote-thread bind lost its worker/session CAS");
            }
        }
        Ok(())
    }

    pub fn observe_dedicated_remote_reattach(
        &self,
        session_id: &str,
        worker_boot_epoch: u64,
        remote_thread_id: &str,
    ) -> Result<()> {
        validate_bounded_runtime_text("dedicated session id", session_id, 256)?;
        validate_bounded_runtime_text("remote thread id", remote_thread_id, 256)?;
        let matched: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM dedicated_session
              WHERE session_id=?1 AND worker_boot_epoch=?2 AND remote_thread_id=?3
                AND state IN ('recovering','idle','outcome_unknown'))",
            params![
                session_id,
                i64::try_from(worker_boot_epoch)?,
                remote_thread_id
            ],
            |row| row.get(0),
        )?;
        if !matched {
            bail!("dedicated remote reattach lost its session/thread/epoch CAS");
        }
        Ok(())
    }

    pub fn settle_dedicated_remote_recovery_status(
        &self,
        session_id: &str,
        worker_boot_epoch: u64,
        remote_thread_id: &str,
        remote_status: &str,
    ) -> Result<()> {
        if !matches!(
            remote_status,
            "idle" | "active" | "notLoaded" | "systemError"
        ) {
            bail!("remote recovery status is outside the pinned vocabulary");
        }
        let next = if remote_status == "idle" {
            "idle"
        } else {
            "outcome_unknown"
        };
        let changed = self.conn.execute(
            "UPDATE dedicated_session SET state=?4, updated_at_ms=?5
              WHERE session_id=?1 AND worker_boot_epoch=?2 AND remote_thread_id=?3
                AND state='recovering'",
            params![
                session_id,
                i64::try_from(worker_boot_epoch)?,
                remote_thread_id,
                next,
                lillux::time::timestamp_millis() as i64
            ],
        )?;
        if changed != 1 {
            let matched: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM dedicated_session
                  WHERE session_id=?1 AND worker_boot_epoch=?2 AND remote_thread_id=?3
                    AND state=?4)",
                params![
                    session_id,
                    i64::try_from(worker_boot_epoch)?,
                    remote_thread_id,
                    next
                ],
                |row| row.get(0),
            )?;
            if !matched {
                bail!("dedicated remote recovery status lost its session/thread/epoch CAS");
            }
        }
        Ok(())
    }

    pub fn prepare_dedicated_session_recovery(
        &self,
        session_id: &str,
        credential_generation: u64,
        credential_lock_owner: &str,
    ) -> Result<u64> {
        let next_epoch: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(boot_epoch), 0) + 1 FROM worker_process WHERE session_id=?1",
            [session_id],
            |row| row.get(0),
        )?;
        let changed = self.conn.execute(
            "UPDATE dedicated_session
                SET state='admitted', credential_generation=?2, updated_at_ms=?3
              WHERE session_id=?1 AND state='recovering'
                AND worker_instance_id IS NULL AND worker_boot_epoch IS NULL
                AND send_boundary='none'
                AND EXISTS(SELECT 1 FROM credential_profile
                  WHERE profile_id=dedicated_session.credential_profile_id
                    AND owner_principal=dedicated_session.owner_principal
                    AND credential_generation=?2 AND lock_owner=?4
                    AND state IN ('unauthenticated','enrolling','confirming','active'))",
            params![
                session_id,
                i64::try_from(credential_generation)?,
                lillux::time::timestamp_millis() as i64,
                credential_lock_owner
            ],
        )?;
        if changed != 1 {
            bail!("dedicated recovery preparation lost its session CAS");
        }
        u64::try_from(next_epoch).context("negative dedicated recovery epoch")
    }

    pub fn observe_dedicated_session_state(
        &self,
        session_id: &str,
        worker_boot_epoch: u64,
        expected: &str,
        next: &str,
        expected_turn_id: Option<&str>,
        next_turn_id: Option<&str>,
    ) -> Result<()> {
        let allowed = matches!(
            (expected, next),
            ("idle", "turn_running")
                | ("turn_running", "idle")
                | ("turn_running", "recovering")
                | ("awaiting_approval", "recovering")
        );
        if !allowed {
            bail!("dedicated worker observation requested an invalid lifecycle edge");
        }
        for turn_id in [expected_turn_id, next_turn_id].into_iter().flatten() {
            validate_bounded_runtime_text("dedicated current turn id", turn_id, 256)?;
        }
        if next == "turn_running" && next_turn_id.is_none() {
            bail!("turn-running observation requires a remote turn id");
        }
        if next == "idle" && (expected_turn_id.is_none() || next_turn_id.is_some()) {
            bail!("idle observation must clear its remote turn id");
        }
        let changed = self.conn.execute(
            "UPDATE dedicated_session SET state=?4, current_turn_id=?6, updated_at_ms=?7
              WHERE session_id=?1 AND worker_boot_epoch=?2 AND state=?3
                AND ((?5 IS NULL AND current_turn_id IS NULL) OR current_turn_id=?5)",
            params![
                session_id,
                i64::try_from(worker_boot_epoch)?,
                expected,
                next,
                expected_turn_id,
                next_turn_id,
                lillux::time::timestamp_millis() as i64
            ],
        )?;
        if changed != 1 {
            let matched: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM dedicated_session
                  WHERE session_id=?1 AND worker_boot_epoch=?2 AND state=?3
                    AND ((?4 IS NULL AND current_turn_id IS NULL) OR current_turn_id=?4))",
                params![
                    session_id,
                    i64::try_from(worker_boot_epoch)?,
                    next,
                    next_turn_id
                ],
                |row| row.get(0),
            )?;
            if !matched {
                bail!("dedicated worker observation lost its session-epoch/state CAS");
            }
        }
        Ok(())
    }

    pub fn create_credential_profile(&self, profile: NewCredentialProfile<'_>) -> Result<()> {
        for (label, value) in [
            ("credential profile id", profile.profile_id),
            ("credential profile owner", profile.owner_principal),
            ("credential profile home id", profile.home_id),
        ] {
            validate_bounded_runtime_text(label, value, 256)?;
        }
        let now = lillux::time::timestamp_millis() as i64;
        let changed = self.conn.execute(
            "INSERT INTO credential_profile (
                profile_id, owner_principal, home_id, credential_generation, state,
                active_login_id, login_epoch, login_expires_at_ms, sanitized_account_json,
                lock_owner, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, 1, 'unauthenticated', NULL, 0, NULL, NULL, NULL, ?4, ?4)",
            params![
                profile.profile_id,
                profile.owner_principal,
                profile.home_id,
                now
            ],
        )?;
        if changed != 1 {
            bail!("credential profile insertion did not create exactly one row");
        }
        Ok(())
    }

    pub fn credential_profile(&self, profile_id: &str) -> Result<Option<CredentialProfileRecord>> {
        validate_bounded_runtime_text("credential profile id", profile_id, 256)?;
        let row = self
            .conn
            .query_row(
                "SELECT profile_id, owner_principal, home_id, credential_generation, state,
                    active_login_id, login_epoch, login_expires_at_ms, sanitized_account_json,
                    lock_owner, created_at_ms, updated_at_ms
               FROM credential_profile WHERE profile_id=?1",
                [profile_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, Option<i64>>(7)?,
                        row.get::<_, Option<String>>(8)?,
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, i64>(10)?,
                        row.get::<_, i64>(11)?,
                    ))
                },
            )
            .optional()?;
        row.map(|row| {
            let sanitized_account = row
                .8
                .map(|raw| serde_json::from_str(&raw))
                .transpose()
                .context("decode sanitized credential account")?;
            Ok(CredentialProfileRecord {
                profile_id: row.0,
                owner_principal: row.1,
                home_id: row.2,
                credential_generation: u64::try_from(row.3)
                    .context("negative credential generation")?,
                state: row.4,
                active_login_id: row.5,
                login_epoch: u64::try_from(row.6).context("negative login epoch")?,
                login_expires_at_ms: row.7,
                sanitized_account,
                lock_owner: row.9,
                created_at_ms: row.10,
                updated_at_ms: row.11,
            })
        })
        .transpose()
    }

    pub fn acquire_credential_profile(
        &self,
        profile_id: &str,
        owner_principal: &str,
        lock_owner: &str,
    ) -> Result<u64> {
        for (label, value) in [
            ("credential profile id", profile_id),
            ("credential profile owner", owner_principal),
            ("credential profile lock owner", lock_owner),
        ] {
            validate_bounded_runtime_text(label, value, 256)?;
        }
        let now = lillux::time::timestamp_millis() as i64;
        let changed = self.conn.execute(
            "UPDATE credential_profile SET lock_owner=?3, updated_at_ms=?4
              WHERE profile_id=?1 AND owner_principal=?2 AND lock_owner IS NULL
                AND state IN ('unauthenticated','enrolling','active')",
            params![profile_id, owner_principal, lock_owner, now],
        )?;
        if changed != 1 {
            bail!("credential profile is absent, not owned, locked, or deleting");
        }
        self.conn
            .query_row(
                "SELECT credential_generation FROM credential_profile WHERE profile_id=?1",
                [profile_id],
                |row| row.get::<_, i64>(0),
            )
            .map_err(Into::into)
            .and_then(|value| u64::try_from(value).context("negative credential generation"))
    }

    pub fn release_credential_profile(&self, profile_id: &str, lock_owner: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE credential_profile SET lock_owner=NULL, updated_at_ms=?3
              WHERE profile_id=?1 AND lock_owner=?2",
            params![
                profile_id,
                lock_owner,
                lillux::time::timestamp_millis() as i64
            ],
        )?;
        if changed != 1 {
            bail!("credential profile lock release lost its CAS");
        }
        Ok(())
    }

    pub fn begin_credential_enrollment(
        &self,
        profile_id: &str,
        lock_owner: &str,
        login_id: &str,
        expires_at_ms: i64,
    ) -> Result<u64> {
        validate_bounded_runtime_text("credential login id", login_id, 256)?;
        let now = lillux::time::timestamp_millis() as i64;
        if expires_at_ms <= now {
            bail!("credential enrollment expiry must be in the future");
        }
        let tx = self.conn.unchecked_transaction()?;
        let epoch: i64 = tx
            .query_row(
                "SELECT login_epoch FROM credential_profile
              WHERE profile_id=?1 AND lock_owner=?2 AND state='unauthenticated'",
                params![profile_id, lock_owner],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("credential profile cannot begin enrollment"))?;
        let next = epoch
            .checked_add(1)
            .ok_or_else(|| anyhow!("credential login epoch overflow"))?;
        tx.execute(
            "UPDATE credential_profile SET state='enrolling', active_login_id=?3,
                    login_epoch=?4, login_expires_at_ms=?5, sanitized_account_json=NULL,
                    updated_at_ms=?6
              WHERE profile_id=?1 AND lock_owner=?2 AND login_epoch=?7",
            params![
                profile_id,
                lock_owner,
                login_id,
                next,
                expires_at_ms,
                now,
                epoch
            ],
        )?;
        tx.commit()?;
        u64::try_from(next).context("negative credential login epoch")
    }

    pub fn complete_credential_enrollment(
        &self,
        profile_id: &str,
        lock_owner: &str,
        login_id: &str,
        login_epoch: u64,
        sanitized_account: &serde_json::Value,
    ) -> Result<u64> {
        let account_json = serde_json::to_string(sanitized_account)?;
        validate_bounded_runtime_text("sanitized credential account", &account_json, 16 * 1024)?;
        let now = lillux::time::timestamp_millis() as i64;
        let login_epoch = i64::try_from(login_epoch).context("login epoch exceeds SQLite range")?;
        let changed = self.conn.execute(
            "UPDATE credential_profile
                SET state='active', active_login_id=NULL, login_expires_at_ms=NULL,
                    sanitized_account_json=?5, credential_generation=credential_generation+1,
                    updated_at_ms=?6
              WHERE profile_id=?1 AND lock_owner=?2 AND active_login_id=?3
                AND login_epoch=?4 AND state='enrolling' AND login_expires_at_ms>=?6",
            params![
                profile_id,
                lock_owner,
                login_id,
                login_epoch,
                account_json,
                now
            ],
        )?;
        if changed != 1 {
            bail!("credential enrollment completion lost its ceremony CAS");
        }
        self.conn
            .query_row(
                "SELECT credential_generation FROM credential_profile WHERE profile_id=?1",
                [profile_id],
                |row| row.get::<_, i64>(0),
            )
            .map_err(Into::into)
            .and_then(|value| u64::try_from(value).context("negative credential generation"))
    }

    pub fn observe_session_credential_enrollment(
        &self,
        session_id: &str,
        worker_instance_id: &str,
        worker_boot_epoch: u64,
        sanitized_account: &serde_json::Value,
    ) -> Result<u64> {
        let account_json = serde_json::to_string(sanitized_account)?;
        validate_bounded_runtime_text("sanitized credential account", &account_json, 16 * 1024)?;
        let now = lillux::time::timestamp_millis() as i64;
        let tx = self.conn.unchecked_transaction()?;
        let profile: (String, String, i64) = tx.query_row(
            "SELECT credential_profile_id, active_login_id, login_epoch
               FROM dedicated_session JOIN credential_profile
                 ON profile_id=credential_profile_id
              WHERE session_id=?1 AND worker_instance_id=?2 AND worker_boot_epoch=?3
                AND credential_profile.lock_owner=?2
                AND credential_profile.state='enrolling'
                AND credential_profile.login_expires_at_ms>=?4",
            params![
                session_id,
                worker_instance_id,
                i64::try_from(worker_boot_epoch)?,
                now
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let profile_changed = tx.execute(
            "UPDATE credential_profile
                SET state='confirming', sanitized_account_json=?4, updated_at_ms=?5
              WHERE profile_id=?1 AND lock_owner=?2 AND active_login_id=?3
                AND state='enrolling'",
            params![profile.0, worker_instance_id, profile.1, account_json, now],
        )?;
        if profile_changed != 1 {
            bail!("session credential observation lost its profile/session CAS");
        }
        tx.commit()?;
        u64::try_from(profile.2).context("negative credential login epoch")
    }

    pub fn confirm_credential_enrollment(
        &self,
        profile_id: &str,
        owner_principal: &str,
        login_epoch: u64,
        expected_account_digest: &str,
    ) -> Result<u64> {
        if !lillux::valid_hash(expected_account_digest) {
            bail!("credential confirmation account digest is not canonical");
        }
        let now = lillux::time::timestamp_millis() as i64;
        let tx = self.conn.unchecked_transaction()?;
        let account_json: String = tx
            .query_row(
                "SELECT sanitized_account_json FROM credential_profile
                  WHERE profile_id=?1 AND owner_principal=?2 AND login_epoch=?3
                    AND state='confirming' AND login_expires_at_ms>=?4
                    AND lock_owner IS NULL",
                params![
                    profile_id,
                    owner_principal,
                    i64::try_from(login_epoch)?,
                    now
                ],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("credential confirmation ceremony is absent or expired"))?;
        let account: serde_json::Value = serde_json::from_str(&account_json)?;
        if ryeos_state::objects::canonical_value_digest(&account)? != expected_account_digest {
            bail!("credential confirmation account digest changed");
        }
        let changed = tx.execute(
            "UPDATE credential_profile
                SET state='active', active_login_id=NULL, login_expires_at_ms=NULL,
                    credential_generation=credential_generation+1, updated_at_ms=?4
              WHERE profile_id=?1 AND owner_principal=?2 AND login_epoch=?3
                AND state='confirming'",
            params![
                profile_id,
                owner_principal,
                i64::try_from(login_epoch)?,
                now
            ],
        )?;
        if changed != 1 {
            bail!("credential confirmation lost its ceremony CAS");
        }
        let generation: i64 = tx.query_row(
            "SELECT credential_generation FROM credential_profile WHERE profile_id=?1",
            [profile_id],
            |row| row.get(0),
        )?;
        tx.commit()?;
        u64::try_from(generation).context("negative credential generation")
    }

    pub fn cancel_credential_enrollment(
        &self,
        profile_id: &str,
        lock_owner: &str,
        login_id: &str,
        login_epoch: u64,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE credential_profile SET state='unauthenticated', active_login_id=NULL,
                    login_expires_at_ms=NULL, updated_at_ms=?5
              WHERE profile_id=?1 AND lock_owner=?2 AND active_login_id=?3
                AND login_epoch=?4 AND state='enrolling'",
            params![
                profile_id,
                lock_owner,
                login_id,
                i64::try_from(login_epoch).context("login epoch exceeds SQLite range")?,
                lillux::time::timestamp_millis() as i64
            ],
        )?;
        if changed != 1 {
            bail!("credential enrollment cancellation lost its ceremony CAS");
        }
        Ok(())
    }

    pub fn revoke_credential_profile(
        &self,
        profile_id: &str,
        owner_principal: &str,
        expected_generation: u64,
    ) -> Result<u64> {
        let expected = i64::try_from(expected_generation)
            .context("credential generation exceeds SQLite range")?;
        let changed = self.conn.execute(
            "UPDATE credential_profile
                SET state='revoking', credential_generation=credential_generation+1,
                    active_login_id=NULL, login_expires_at_ms=NULL,
                    sanitized_account_json=NULL, updated_at_ms=?4
              WHERE profile_id=?1 AND owner_principal=?2 AND credential_generation=?3
                AND state NOT IN ('revoking','revoked','deleting')",
            params![
                profile_id,
                owner_principal,
                expected,
                lillux::time::timestamp_millis() as i64
            ],
        )?;
        if changed != 1 {
            bail!("credential revocation lost its owner/generation CAS");
        }
        Ok(expected_generation + 1)
    }

    pub fn finish_credential_profile_revocation(
        &self,
        profile_id: &str,
        owner_principal: &str,
        revoking_generation: u64,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE credential_profile
                SET state='revoked', lock_owner=NULL, updated_at_ms=?4
              WHERE profile_id=?1 AND owner_principal=?2 AND credential_generation=?3
                AND state='revoking'",
            params![
                profile_id,
                owner_principal,
                i64::try_from(revoking_generation)?,
                lillux::time::timestamp_millis() as i64
            ],
        )?;
        if changed != 1 {
            bail!("credential revocation finalization lost its generation CAS");
        }
        Ok(())
    }

    pub fn begin_credential_profile_deletion(
        &self,
        profile_id: &str,
        owner_principal: &str,
        expected_generation: u64,
    ) -> Result<u64> {
        let expected = i64::try_from(expected_generation)
            .context("credential generation exceeds SQLite range")?;
        let tx = self.conn.unchecked_transaction()?;
        let live_sessions: i64 = tx.query_row(
            "SELECT COUNT(*) FROM dedicated_session
              WHERE credential_profile_id=?1 AND state!='terminal'",
            [profile_id],
            |row| row.get(0),
        )?;
        if live_sessions != 0 {
            bail!("credential profile still owns nonterminal sessions");
        }
        let changed = tx.execute(
            "UPDATE credential_profile
                SET state='deleting', credential_generation=credential_generation+1,
                    active_login_id=NULL, login_expires_at_ms=NULL,
                    sanitized_account_json=NULL, lock_owner=NULL, updated_at_ms=?4
              WHERE profile_id=?1 AND owner_principal=?2 AND credential_generation=?3
                AND state IN ('unauthenticated','revoked')",
            params![
                profile_id,
                owner_principal,
                expected,
                lillux::time::timestamp_millis() as i64
            ],
        )?;
        if changed != 1 {
            bail!("credential profile deletion lost its owner/generation CAS");
        }
        tx.commit()?;
        Ok(expected_generation + 1)
    }

    pub fn finish_credential_profile_deletion(
        &self,
        profile_id: &str,
        owner_principal: &str,
        deleting_generation: u64,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "DELETE FROM credential_profile
              WHERE profile_id=?1 AND owner_principal=?2 AND credential_generation=?3
                AND state='deleting'",
            params![
                profile_id,
                owner_principal,
                i64::try_from(deleting_generation)
                    .context("credential generation exceeds SQLite range")?
            ],
        )?;
        if changed != 1 {
            bail!("credential profile deletion finalization lost its generation CAS");
        }
        Ok(())
    }

    pub fn reserve_dedicated_session_command(
        &self,
        command: NewDedicatedSessionCommand<'_>,
    ) -> Result<DedicatedSessionCommandRecord> {
        for (label, value, max) in [
            ("command session id", command.session_id, 256),
            ("command idempotency key", command.idempotency_key, 256),
            ("command kind", command.command_kind, 128),
            ("command request digest", command.request_digest, 128),
        ] {
            validate_bounded_runtime_text(label, value, max)?;
        }
        let payload_json = serde_json::to_string(command.payload)?;
        validate_bounded_runtime_text("command payload", &payload_json, 256 * 1024)?;
        let epoch = i64::try_from(command.worker_boot_epoch)
            .context("worker boot epoch exceeds SQLite range")?;
        let tx = self.conn.unchecked_transaction()?;
        if let Some(existing) =
            read_dedicated_command_by_key(&tx, command.session_id, command.idempotency_key)?
        {
            if existing.command_kind != command.command_kind
                || existing.request_digest != command.request_digest
                || existing.payload != *command.payload
            {
                bail!("command idempotency key was reused for different authority");
            }
            if existing.worker_boot_epoch != command.worker_boot_epoch
                && !(existing.state == "failed"
                    && existing.result.as_ref().and_then(|value| {
                        value
                            .get("retryable_uncontacted")
                            .and_then(serde_json::Value::as_bool)
                    }) == Some(true))
            {
                bail!("command idempotency key belongs to a different contacted worker epoch");
            }
            tx.commit()?;
            return Ok(existing);
        }
        let next: i64 = tx.query_row(
            "SELECT COALESCE(MAX(command_sequence), 0) + 1 FROM dedicated_session_command WHERE session_id=?1",
            [command.session_id], |row| row.get(0),
        )?;
        if next > MAX_DEDICATED_SESSION_COMMANDS {
            bail!("dedicated session command ledger reached its count ceiling");
        }
        let spool_bytes: i64 = tx.query_row(
            "SELECT COALESCE(SUM(length(payload_json) + COALESCE(length(result_json), 0)), 0)
               FROM dedicated_session_command WHERE session_id=?1",
            [command.session_id],
            |row| row.get(0),
        )?;
        let reserved_bytes = i64::try_from(payload_json.len())?
            .checked_add(256 * 1024)
            .context("dedicated command reservation byte overflow")?;
        if spool_bytes
            .checked_add(reserved_bytes)
            .is_none_or(|total| total > MAX_DEDICATED_SESSION_COMMAND_SPOOL_BYTES)
        {
            bail!("dedicated session command/output spool reached its byte ceiling");
        }
        let now = lillux::time::timestamp_millis() as i64;
        let active_commands: i64 = tx.query_row(
            "SELECT COUNT(*) FROM dedicated_session_command
              WHERE session_id=?1 AND state IN ('committed','dispatched')",
            [command.session_id],
            |row| row.get(0),
        )?;
        if active_commands != 0 {
            bail!("dedicated session already has an unsettled command");
        }
        let session_changed = tx.execute(
            "UPDATE dedicated_session SET send_boundary='committed', updated_at_ms=?3
              WHERE session_id=?1 AND worker_boot_epoch=?2
                AND state IN ('idle','turn_running','awaiting_approval','recovering')
                AND send_boundary IN ('none','settled')
                AND EXISTS(SELECT 1 FROM credential_profile
                    WHERE profile_id=dedicated_session.credential_profile_id
                      AND credential_generation=dedicated_session.credential_generation
                      AND lock_owner=dedicated_session.worker_instance_id
                      AND state IN ('unauthenticated','enrolling','confirming','active'))",
            params![command.session_id, epoch, now],
        )?;
        if session_changed != 1 {
            bail!("command admission lost its idle worker-epoch CAS");
        }
        tx.execute(
            "INSERT INTO dedicated_session_command (
                session_id, command_sequence, idempotency_key, worker_boot_epoch,
                command_kind, request_digest, payload_json, state, result_json,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'committed', NULL, ?8, ?8)",
            params![
                command.session_id,
                next,
                command.idempotency_key,
                epoch,
                command.command_kind,
                command.request_digest,
                payload_json,
                now
            ],
        )?;
        let record =
            read_dedicated_command_by_key(&tx, command.session_id, command.idempotency_key)?
                .ok_or_else(|| anyhow!("committed command disappeared"))?;
        tx.commit()?;
        Ok(record)
    }

    /// Durable command-outbox rows whose canonical root testimony may need to
    /// be completed after a daemon crash. The root chain remains authoritative;
    /// these rows only retain enough exact material to idempotently finish it.
    pub fn dedicated_command_outbox_records(&self) -> Result<Vec<DedicatedSessionCommandRecord>> {
        let mut statement = self.conn.prepare(
            "SELECT session_id, idempotency_key
               FROM dedicated_session_command
              WHERE state IN ('committed','dispatched','outcome_unknown','failed')
              ORDER BY session_id, command_sequence",
        )?;
        let identities = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        identities
            .into_iter()
            .map(|(session_id, idempotency_key)| {
                read_dedicated_command_by_key(&self.conn, &session_id, &idempotency_key)?
                    .ok_or_else(|| anyhow!("listed dedicated command outbox row disappeared"))
            })
            .collect()
    }

    pub fn mark_dedicated_command_contacted(
        &self,
        session_id: &str,
        command_sequence: u64,
        worker_boot_epoch: u64,
    ) -> Result<()> {
        let now = lillux::time::timestamp_millis() as i64;
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE dedicated_session_command SET state='dispatched', updated_at_ms=?4
              WHERE session_id=?1 AND command_sequence=?2 AND worker_boot_epoch=?3 AND state='committed'",
            params![session_id, i64::try_from(command_sequence)?, i64::try_from(worker_boot_epoch)?, now],
        )?;
        let session_changed = tx.execute(
            "UPDATE dedicated_session SET send_boundary='contacted', updated_at_ms=?3
              WHERE session_id=?1 AND worker_boot_epoch=?2
                AND state IN ('idle','turn_running','awaiting_approval','recovering')
                AND send_boundary='committed'
                AND EXISTS(SELECT 1 FROM credential_profile
                    WHERE profile_id=dedicated_session.credential_profile_id
                      AND credential_generation=dedicated_session.credential_generation
                      AND lock_owner=dedicated_session.worker_instance_id
                      AND state IN ('unauthenticated','enrolling','confirming','active'))",
            params![session_id, i64::try_from(worker_boot_epoch)?, now],
        )?;
        if changed != 1 || session_changed != 1 {
            bail!("command contact lost its command/session CAS");
        }
        tx.commit()?;
        Ok(())
    }

    pub fn settle_dedicated_command(
        &self,
        session_id: &str,
        command_sequence: u64,
        worker_boot_epoch: u64,
        succeeded: bool,
        result: &serde_json::Value,
    ) -> Result<()> {
        let result_json = serde_json::to_string(result)?;
        validate_bounded_runtime_text("command result", &result_json, 256 * 1024)?;
        let now = lillux::time::timestamp_millis() as i64;
        let tx = self.conn.unchecked_transaction()?;
        let state = if succeeded { "completed" } else { "failed" };
        let changed = tx.execute(
            "UPDATE dedicated_session_command SET state=?4, result_json=?5, updated_at_ms=?6
              WHERE session_id=?1 AND command_sequence=?2 AND worker_boot_epoch=?3 AND state='dispatched'",
            params![session_id, i64::try_from(command_sequence)?, i64::try_from(worker_boot_epoch)?, state, result_json, now],
        )?;
        let session_changed = tx.execute(
            "UPDATE dedicated_session SET send_boundary='settled', updated_at_ms=?3
              WHERE session_id=?1 AND worker_boot_epoch=?2
                AND state IN ('idle','turn_running','awaiting_approval','recovering')
                AND send_boundary='contacted'",
            params![session_id, i64::try_from(worker_boot_epoch)?, now],
        )?;
        if changed != 1 || session_changed != 1 {
            bail!("command settlement lost its command/session CAS");
        }
        tx.commit()?;
        Ok(())
    }

    /// Settle a command whose authoritative response batch was recovered
    /// from the root event chain after a daemon crash. The response body is
    /// intentionally not reconstructed; only its retained redacted digest is
    /// projected back into the outbox.
    pub fn settle_recovered_dedicated_command(
        &self,
        session_id: &str,
        command_sequence: u64,
        worker_boot_epoch: u64,
        result: &serde_json::Value,
    ) -> Result<()> {
        let result_json = serde_json::to_string(result)?;
        validate_bounded_runtime_text("recovered command result", &result_json, 256 * 1024)?;
        let now = lillux::time::timestamp_millis() as i64;
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE dedicated_session_command SET state='completed', result_json=?4, updated_at_ms=?5
              WHERE session_id=?1 AND command_sequence=?2 AND worker_boot_epoch=?3
                AND state IN ('dispatched','outcome_unknown')",
            params![session_id, i64::try_from(command_sequence)?, i64::try_from(worker_boot_epoch)?, result_json, now],
        )?;
        let session_changed = tx.execute(
            "UPDATE dedicated_session SET send_boundary='settled',
                    state=CASE WHEN state='outcome_unknown' THEN 'idle' ELSE state END,
                    updated_at_ms=?3
              WHERE session_id=?1 AND worker_boot_epoch=?2
                AND state IN ('idle','turn_running','awaiting_approval','recovering','outcome_unknown')
                AND send_boundary IN ('contacted','outcome_unknown')",
            params![session_id, i64::try_from(worker_boot_epoch)?, now],
        )?;
        if changed != 1 || session_changed != 1 {
            bail!("recovered command settlement lost its command/session CAS");
        }
        tx.commit()?;
        Ok(())
    }

    /// Project an authoritative response for a historical worker epoch after
    /// the owning root is already terminal.  The terminal root is the caller's
    /// authority for this transition; the current session projection may have
    /// detached or advanced past the dead worker epoch, so it must not be
    /// rewritten while repairing the exact command row.
    pub fn settle_terminal_recovered_dedicated_command(
        &self,
        session_id: &str,
        command_sequence: u64,
        worker_boot_epoch: u64,
        result: &serde_json::Value,
    ) -> Result<()> {
        let result_json = serde_json::to_string(result)?;
        validate_bounded_runtime_text("recovered command result", &result_json, 256 * 1024)?;
        let changed = self.conn.execute(
            "UPDATE dedicated_session_command
                SET state='completed', result_json=?4, updated_at_ms=?5
              WHERE session_id=?1 AND command_sequence=?2 AND worker_boot_epoch=?3
                AND state IN ('dispatched','outcome_unknown')
                AND EXISTS(SELECT 1 FROM dedicated_session WHERE session_id=?1)",
            params![
                session_id,
                i64::try_from(command_sequence)?,
                i64::try_from(worker_boot_epoch)?,
                result_json,
                lillux::time::timestamp_millis() as i64
            ],
        )?;
        if changed != 1 {
            bail!("terminal recovered command settlement lost its exact command CAS");
        }
        Ok(())
    }

    pub fn mark_dedicated_command_outcome_unknown(
        &self,
        session_id: &str,
        command_sequence: u64,
        worker_boot_epoch: u64,
    ) -> Result<()> {
        let now = lillux::time::timestamp_millis() as i64;
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE dedicated_session_command SET state='outcome_unknown', updated_at_ms=?4
              WHERE session_id=?1 AND command_sequence=?2 AND worker_boot_epoch=?3 AND state='dispatched'",
            params![session_id, i64::try_from(command_sequence)?, i64::try_from(worker_boot_epoch)?, now],
        )?;
        let session_changed = tx.execute(
            "UPDATE dedicated_session SET state='outcome_unknown', send_boundary='outcome_unknown', updated_at_ms=?3
              WHERE session_id=?1 AND worker_boot_epoch=?2 AND send_boundary='contacted'",
            params![session_id, i64::try_from(worker_boot_epoch)?, now],
        )?;
        if changed != 1 || session_changed != 1 {
            bail!("ambiguous command reconciliation lost its CAS");
        }
        tx.commit()?;
        Ok(())
    }

    /// Reserve the possible-contact boundary before appending a worker batch
    /// to the authoritative root thread event chain. A contacting row is never
    /// automatically retried: after a crash RyeOS cannot prove whether the CAS
    /// append happened.
    pub fn reserve_dedicated_observation_batch(
        &self,
        session_id: &str,
        worker_boot_epoch: u64,
        first_sequence: u64,
        through_sequence: u64,
        previous_digest: Option<&str>,
        batch_digest: &str,
    ) -> Result<ObservationBatchReservation> {
        validate_bounded_runtime_text("dedicated session id", session_id, 256)?;
        validate_bounded_runtime_text("observation batch digest", batch_digest, 128)?;
        if let Some(previous) = previous_digest {
            validate_bounded_runtime_text("previous observation digest", previous, 128)?;
        }
        if worker_boot_epoch == 0
            || first_sequence == 0
            || through_sequence < first_sequence
            || through_sequence - first_sequence >= 512
        {
            bail!("observation batch sequence range is invalid or unbounded");
        }
        let epoch = i64::try_from(worker_boot_epoch)?;
        let first = i64::try_from(first_sequence)?;
        let through = i64::try_from(through_sequence)?;
        let tx = self.conn.unchecked_transaction()?;
        let attached: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM dedicated_session
              WHERE session_id=?1 AND worker_boot_epoch=?2 AND state!='terminal')",
            params![session_id, epoch],
            |row| row.get(0),
        )?;
        if !attached {
            bail!("observation batch lost its live worker epoch");
        }
        let existing = tx
            .query_row(
                "SELECT through_sequence, previous_digest, batch_digest, state
                   FROM dedicated_session_observation_batch
                  WHERE session_id=?1 AND worker_boot_epoch=?2 AND first_sequence=?3",
                params![session_id, epoch, first],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing.0 != through
                || existing.1.as_deref() != previous_digest
                || existing.2 != batch_digest
            {
                bail!("observation batch sequence identity was reused with different content");
            }
            return match existing.3.as_str() {
                "settled" => Ok(ObservationBatchReservation::AlreadySettled),
                "append_contacting" | "append_unknown" => {
                    Ok(ObservationBatchReservation::RebuildProjection)
                }
                _ => bail!("observation batch has an invalid durable state"),
            };
        }
        let predecessor = tx
            .query_row(
                "SELECT through_sequence, batch_digest, state
                   FROM dedicated_session_observation_batch
                  WHERE session_id=?1 AND worker_boot_epoch=?2
                  ORDER BY through_sequence DESC LIMIT 1",
                params![session_id, epoch],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        match predecessor {
            None if first_sequence == 1 && previous_digest.is_none() => {}
            Some((prior_through, prior_digest, prior_state))
                if prior_state == "settled"
                    && prior_through.checked_add(1) == Some(first)
                    && previous_digest == Some(prior_digest.as_str()) => {}
            Some((_, _, prior_state)) if prior_state != "settled" => {
                bail!("prior observation append outcome is unknown")
            }
            _ => bail!("observation batch has a gap, reordering, or broken digest chain"),
        }
        let now = lillux::time::timestamp_millis() as i64;
        tx.execute(
            "INSERT INTO dedicated_session_observation_batch(
                session_id, worker_boot_epoch, first_sequence, through_sequence,
                previous_digest, batch_digest, state, created_at_ms, settled_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'append_contacting', ?7, NULL)",
            params![
                session_id,
                epoch,
                first,
                through,
                previous_digest,
                batch_digest,
                now
            ],
        )?;
        tx.commit()?;
        Ok(ObservationBatchReservation::ContactAppend)
    }

    pub fn settle_dedicated_observation_batch(
        &self,
        session_id: &str,
        worker_boot_epoch: u64,
        first_sequence: u64,
        batch_digest: &str,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE dedicated_session_observation_batch
                SET state='settled', settled_at_ms=?5
              WHERE session_id=?1 AND worker_boot_epoch=?2 AND first_sequence=?3
                AND batch_digest=?4 AND state IN ('append_contacting','append_unknown')",
            params![
                session_id,
                i64::try_from(worker_boot_epoch)?,
                i64::try_from(first_sequence)?,
                batch_digest,
                lillux::time::timestamp_millis() as i64
            ],
        )?;
        if changed != 1 {
            bail!("observation batch lost its append-contacting reservation");
        }
        Ok(())
    }

    /// Return pushed-observation reservations whose authoritative append or
    /// rebuildable projection was interrupted. Startup repairs these while the
    /// exact old worker epoch is still retained and after its process has been
    /// quiesced.
    pub fn dedicated_observation_outbox_records(
        &self,
    ) -> Result<Vec<DedicatedObservationBatchRecord>> {
        let mut statement = self.conn.prepare(
            "SELECT session_id, worker_boot_epoch, first_sequence,
                    through_sequence, batch_digest, state
               FROM dedicated_session_observation_batch
              WHERE state IN ('append_contacting','append_unknown')
              ORDER BY session_id, worker_boot_epoch, first_sequence",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?
            .map(|row| {
                let row = row?;
                Ok(DedicatedObservationBatchRecord {
                    session_id: row.0,
                    worker_boot_epoch: u64::try_from(row.1)?,
                    first_sequence: u64::try_from(row.2)?,
                    through_sequence: u64::try_from(row.3)?,
                    batch_digest: row.4,
                    state: row.5,
                })
            })
            .collect()
    }

    /// Remove an exact append reservation only after the startup reconciler
    /// has quiesced the old worker and proved that the immutable root chain has
    /// no corresponding batch. No authority was published, so the reservation
    /// itself is rebuildable and may be discarded.
    pub fn discard_unappended_dedicated_observation_batch(
        &self,
        session_id: &str,
        worker_boot_epoch: u64,
        first_sequence: u64,
        batch_digest: &str,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "DELETE FROM dedicated_session_observation_batch
              WHERE session_id=?1 AND worker_boot_epoch=?2 AND first_sequence=?3
                AND batch_digest=?4 AND state IN ('append_contacting','append_unknown')",
            params![
                session_id,
                i64::try_from(worker_boot_epoch)?,
                i64::try_from(first_sequence)?,
                batch_digest,
            ],
        )?;
        if changed != 1 {
            bail!("unappended observation discard lost its exact reservation CAS");
        }
        Ok(())
    }

    pub fn mark_dedicated_observation_batch_unknown(
        &self,
        session_id: &str,
        worker_boot_epoch: u64,
        first_sequence: u64,
        batch_digest: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE dedicated_session_observation_batch SET state='append_unknown'
              WHERE session_id=?1 AND worker_boot_epoch=?2 AND first_sequence=?3
                AND batch_digest=?4 AND state='append_contacting'",
            params![
                session_id,
                i64::try_from(worker_boot_epoch)?,
                i64::try_from(first_sequence)?,
                batch_digest
            ],
        )?;
        Ok(())
    }

    pub fn create_dedicated_session_approval(
        &self,
        approval: NewDedicatedSessionApproval<'_>,
    ) -> Result<()> {
        let authority_json = serde_json::to_string(approval.requested_authority)?;
        validate_bounded_runtime_text("approval authority", &authority_json, 64 * 1024)?;
        let now = lillux::time::timestamp_millis() as i64;
        if approval.expires_at_ms <= now {
            bail!("approval expiry must be in the future");
        }
        let tx = self.conn.unchecked_transaction()?;
        let existing: Option<(String, String, String, i64)> = tx
            .query_row(
                "SELECT request_digest, operation_class, requested_authority_json, worker_boot_epoch
                   FROM dedicated_session_approval WHERE session_id=?1 AND approval_id=?2",
                params![approval.session_id, approval.approval_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing.0 == approval.request_digest
                && existing.1 == approval.operation_class
                && existing.2 == authority_json
                && existing.3 == i64::try_from(approval.worker_boot_epoch)?
            {
                return Ok(());
            }
            bail!("approval identity was reused with different authority");
        }
        let session_live: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM dedicated_session
              WHERE session_id=?1 AND worker_instance_id=?2 AND worker_boot_epoch=?3
                AND state='turn_running')",
            params![
                approval.session_id,
                approval.worker_instance_id,
                i64::try_from(approval.worker_boot_epoch)?,
            ],
            |row| row.get(0),
        )?;
        if !session_live {
            bail!("approval admission lost its active worker/session CAS");
        }
        tx.execute(
            "INSERT INTO dedicated_session_approval (
                session_id, approval_id, worker_instance_id, worker_boot_epoch,
                request_digest, operation_class, requested_authority_json, state,
                decision_principal, decision_json, decision_digest, reservation_token,
                expires_at_ms, created_at_ms, resolved_at_ms,
                delivery_contacted_at_ms, delivery_settled_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending',
                       NULL, NULL, NULL, NULL, ?8, ?9, NULL, NULL, NULL)",
            params![
                approval.session_id,
                approval.approval_id,
                approval.worker_instance_id,
                i64::try_from(approval.worker_boot_epoch)?,
                approval.request_digest,
                approval.operation_class,
                authority_json,
                approval.expires_at_ms,
                now
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn pending_dedicated_session_approvals(
        &self,
        session_id: &str,
    ) -> Result<Vec<DedicatedSessionApprovalRecord>> {
        validate_bounded_runtime_text("approval session id", session_id, 256)?;
        let mut statement = self.conn.prepare(
            "SELECT session_id, approval_id, worker_instance_id, worker_boot_epoch,
                    request_digest, operation_class, requested_authority_json, state,
                    decision_principal, decision_json, decision_digest, reservation_token,
                    expires_at_ms, created_at_ms, resolved_at_ms,
                    delivery_contacted_at_ms, delivery_settled_at_ms
               FROM dedicated_session_approval
              WHERE session_id=?1
                AND state IN ('pending','decision_reserved','delivery_contacting','delivery_unknown')
              ORDER BY created_at_ms, approval_id",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, i64>(12)?,
                row.get::<_, i64>(13)?,
                row.get::<_, Option<i64>>(14)?,
                row.get::<_, Option<i64>>(15)?,
                row.get::<_, Option<i64>>(16)?,
            ))
        })?;
        rows.map(|row| {
            let row = row?;
            Ok(DedicatedSessionApprovalRecord {
                session_id: row.0,
                approval_id: row.1,
                worker_instance_id: row.2,
                worker_boot_epoch: u64::try_from(row.3)?,
                request_digest: row.4,
                operation_class: row.5,
                requested_authority: serde_json::from_str(&row.6)?,
                state: row.7,
                decision_principal: row.8,
                decision: row
                    .9
                    .map(|value| serde_json::from_str(&value))
                    .transpose()?,
                decision_digest: row.10,
                reservation_token: row.11,
                expires_at_ms: row.12,
                created_at_ms: row.13,
                resolved_at_ms: row.14,
                delivery_contacted_at_ms: row.15,
                delivery_settled_at_ms: row.16,
            })
        })
        .collect()
    }

    pub fn reconcile_dedicated_approval_delivery_unknown(
        &self,
        session_id: &str,
        approval_id: &str,
        worker_boot_epoch: u64,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE dedicated_session_approval
                SET state='delivery_unknown', resolved_at_ms=?4
              WHERE session_id=?1 AND approval_id=?2 AND worker_boot_epoch=?3
                AND state IN ('decision_reserved','delivery_contacting')",
            params![
                session_id,
                approval_id,
                i64::try_from(worker_boot_epoch)?,
                lillux::time::timestamp_millis() as i64
            ],
        )?;
        if changed != 1 {
            let retry: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM dedicated_session_approval
                  WHERE session_id=?1 AND approval_id=?2 AND worker_boot_epoch=?3
                    AND state='delivery_unknown')",
                params![session_id, approval_id, i64::try_from(worker_boot_epoch)?],
                |row| row.get(0),
            )?;
            if !retry {
                bail!("approval outbox reconciliation lost its identity/state CAS");
            }
        }
        Ok(())
    }

    /// Retire a decision that was durably reserved but provably never crossed
    /// the worker-contact boundary. This is distinct from delivery-unknown:
    /// no external effect is possible, and a terminal root cannot accept new
    /// delivery facts for the historical epoch.
    pub fn reconcile_dedicated_approval_stale_epoch(
        &self,
        session_id: &str,
        approval_id: &str,
        worker_boot_epoch: u64,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE dedicated_session_approval
                SET state='stale_epoch', resolved_at_ms=?4
              WHERE session_id=?1 AND approval_id=?2 AND worker_boot_epoch=?3
                AND state='decision_reserved'",
            params![
                session_id,
                approval_id,
                i64::try_from(worker_boot_epoch)?,
                lillux::time::timestamp_millis() as i64
            ],
        )?;
        if changed != 1 {
            let retry: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM dedicated_session_approval
                  WHERE session_id=?1 AND approval_id=?2 AND worker_boot_epoch=?3
                    AND state='stale_epoch')",
                params![session_id, approval_id, i64::try_from(worker_boot_epoch)?],
                |row| row.get(0),
            )?;
            if !retry {
                bail!("approval stale-epoch reconciliation lost its identity/state CAS");
            }
        }
        Ok(())
    }

    /// Atomically closes command and approval admission before retirement.
    /// A retained draining reservation is safe to retry after a crash.
    pub fn reserve_dedicated_session_completion(
        &self,
        session_id: &str,
        worker_boot_epoch: u64,
    ) -> Result<()> {
        let epoch = i64::try_from(worker_boot_epoch)?;
        let now = lillux::time::timestamp_millis() as i64;
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE dedicated_session SET state='draining', updated_at_ms=?3
              WHERE session_id=?1 AND worker_boot_epoch=?2 AND state='idle'
                AND current_turn_id IS NULL
                AND NOT EXISTS(SELECT 1 FROM dedicated_session_command
                    WHERE session_id=?1 AND worker_boot_epoch=?2
                      AND state IN ('committed','dispatched','outcome_unknown'))
                AND NOT EXISTS(SELECT 1 FROM dedicated_session_approval
                    WHERE session_id=?1 AND worker_boot_epoch=?2
                      AND state IN ('pending','decision_reserved','delivery_contacting','delivery_unknown'))
                AND NOT EXISTS(SELECT 1 FROM dedicated_session_observation_batch
                    WHERE session_id=?1 AND worker_boot_epoch=?2 AND state!='settled')",
            params![session_id, epoch, now],
        )?;
        if changed == 0 {
            let retry: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM dedicated_session
                  WHERE session_id=?1 AND worker_boot_epoch=?2 AND state='draining')",
                params![session_id, epoch],
                |row| row.get(0),
            )?;
            if !retry {
                bail!("dedicated completion requires an idle, quiescent worker");
            }
        }
        let worker_changed = tx.execute(
            "UPDATE worker_process SET state='draining', cleanup_state='draining', updated_at_ms=?3
              WHERE session_id=?1 AND boot_epoch=?2 AND state='live' AND cleanup_state='owned'",
            params![session_id, epoch, now],
        )?;
        if worker_changed == 0 {
            let retry: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM worker_process
                  WHERE session_id=?1 AND boot_epoch=?2 AND state='draining'
                    AND cleanup_state='draining')",
                params![session_id, epoch],
                |row| row.get(0),
            )?;
            if !retry {
                bail!("dedicated completion lost its live worker reservation");
            }
        }
        tx.commit()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn reserve_dedicated_session_approval_decision(
        &self,
        session_id: &str,
        approval_id: &str,
        worker_boot_epoch: u64,
        request_digest: &str,
        decision_principal: &str,
        decision: &serde_json::Value,
        decision_digest: &str,
        reservation_token: &str,
    ) -> Result<()> {
        let decision_json = lillux::canonical_json(decision)?;
        validate_bounded_runtime_text("approval decision", &decision_json, 64 * 1024)?;
        validate_bounded_runtime_text("approval decision digest", decision_digest, 128)?;
        validate_bounded_runtime_text("approval reservation token", reservation_token, 256)?;
        let now = lillux::time::timestamp_millis() as i64;
        let changed = self.conn.execute(
            "UPDATE dedicated_session_approval
                SET state='decision_reserved', decision_principal=?5,
                    decision_json=?6, decision_digest=?7, reservation_token=?8,
                    resolved_at_ms=?9
              WHERE session_id=?1 AND approval_id=?2 AND worker_boot_epoch=?3
                AND request_digest=?4 AND state='pending' AND expires_at_ms>=?9",
            params![
                session_id,
                approval_id,
                i64::try_from(worker_boot_epoch)?,
                request_digest,
                decision_principal,
                decision_json,
                decision_digest,
                reservation_token,
                now,
            ],
        )?;
        if changed != 1 {
            bail!("approval resolution lost its digest/epoch/single-use CAS");
        }
        Ok(())
    }

    pub fn mark_dedicated_approval_delivery_contacting(
        &self,
        session_id: &str,
        approval_id: &str,
        worker_boot_epoch: u64,
        reservation_token: &str,
        decision_digest: &str,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE dedicated_session_approval
                SET state='delivery_contacting', delivery_contacted_at_ms=?6
              WHERE session_id=?1 AND approval_id=?2 AND worker_boot_epoch=?3
                AND reservation_token=?4 AND decision_digest=?5
                AND state='decision_reserved'
                AND EXISTS(SELECT 1 FROM dedicated_session
                  JOIN credential_profile ON profile_id=credential_profile_id
                  WHERE dedicated_session.session_id=?1
                    AND dedicated_session.worker_boot_epoch=?3
                    AND dedicated_session.worker_instance_id=dedicated_session_approval.worker_instance_id
                    AND dedicated_session.state IN ('turn_running','awaiting_approval')
                    AND credential_profile.state='active'
                    AND credential_profile.credential_generation=dedicated_session.credential_generation
                    AND credential_profile.lock_owner=dedicated_session.worker_instance_id)",
            params![
                session_id,
                approval_id,
                i64::try_from(worker_boot_epoch)?,
                reservation_token,
                decision_digest,
                lillux::time::timestamp_millis() as i64,
            ],
        )?;
        if changed != 1 {
            bail!("approval delivery lost its reserved pre-contact CAS");
        }
        Ok(())
    }

    pub fn settle_dedicated_approval_delivery(
        &self,
        session_id: &str,
        approval_id: &str,
        worker_boot_epoch: u64,
        reservation_token: &str,
        decision_digest: &str,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE dedicated_session_approval
                SET state='delivery_settled', delivery_settled_at_ms=?6
              WHERE session_id=?1 AND approval_id=?2 AND worker_boot_epoch=?3
                AND reservation_token=?4 AND decision_digest=?5
                AND state='delivery_contacting'",
            params![
                session_id,
                approval_id,
                i64::try_from(worker_boot_epoch)?,
                reservation_token,
                decision_digest,
                lillux::time::timestamp_millis() as i64,
            ],
        )?;
        if changed != 1 {
            bail!("approval delivery lost its contacted CAS");
        }
        Ok(())
    }

    /// Rebuild approval settlement from an exact authoritative root fact. The
    /// fact can outlive both an interrupted SQLite settlement and detachment of
    /// its historical worker epoch, so this repairs only the approval row.
    pub fn settle_recovered_dedicated_approval_delivery(
        &self,
        session_id: &str,
        approval_id: &str,
        worker_boot_epoch: u64,
        reservation_token: &str,
        decision_digest: &str,
    ) -> Result<()> {
        let epoch = i64::try_from(worker_boot_epoch)?;
        let now = lillux::time::timestamp_millis() as i64;
        let changed = self.conn.execute(
            "UPDATE dedicated_session_approval
                SET state='delivery_settled', delivery_settled_at_ms=?6
              WHERE session_id=?1 AND approval_id=?2 AND worker_boot_epoch=?3
                AND reservation_token=?4 AND decision_digest=?5
                AND state IN ('decision_reserved','delivery_contacting','delivery_unknown')",
            params![
                session_id,
                approval_id,
                epoch,
                reservation_token,
                decision_digest,
                now,
            ],
        )?;
        if changed == 1 {
            return Ok(());
        }
        let settled: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM dedicated_session_approval
              WHERE session_id=?1 AND approval_id=?2 AND worker_boot_epoch=?3
                AND reservation_token=?4 AND decision_digest=?5
                AND state='delivery_settled')",
            params![
                session_id,
                approval_id,
                epoch,
                reservation_token,
                decision_digest,
            ],
            |row| row.get(0),
        )?;
        if !settled {
            bail!("recovered approval settlement lost its exact decision CAS");
        }
        Ok(())
    }

    pub fn mark_dedicated_approval_delivery_unknown(
        &self,
        session_id: &str,
        approval_id: &str,
        worker_boot_epoch: u64,
        reservation_token: &str,
        decision_digest: &str,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE dedicated_session_approval SET state='delivery_unknown'
              WHERE session_id=?1 AND approval_id=?2 AND worker_boot_epoch=?3
                AND reservation_token=?4 AND decision_digest=?5
                AND state='delivery_contacting'",
            params![
                session_id,
                approval_id,
                i64::try_from(worker_boot_epoch)?,
                reservation_token,
                decision_digest,
            ],
        )?;
        if changed != 1 {
            bail!("approval unknown settlement lost its contacted CAS");
        }
        Ok(())
    }

    pub fn expire_dedicated_session_approval(
        &self,
        session_id: &str,
        approval_id: &str,
        worker_boot_epoch: u64,
    ) -> Result<()> {
        validate_bounded_runtime_text("approval session id", session_id, 256)?;
        validate_bounded_runtime_text("approval id", approval_id, 256)?;
        let now = lillux::time::timestamp_millis() as i64;
        let approval_changed = self.conn.execute(
            "UPDATE dedicated_session_approval
                SET state='expired', resolved_at_ms=?4
              WHERE session_id=?1 AND approval_id=?2 AND worker_boot_epoch=?3
                AND state='pending' AND expires_at_ms<=?4",
            params![
                session_id,
                approval_id,
                i64::try_from(worker_boot_epoch)?,
                now
            ],
        )?;
        if approval_changed != 1 {
            let expired: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM dedicated_session_approval
                  WHERE session_id=?1 AND approval_id=?2 AND worker_boot_epoch=?3
                    AND state='expired')",
                params![session_id, approval_id, i64::try_from(worker_boot_epoch)?],
                |row| row.get(0),
            )?;
            if !expired {
                bail!("approval expiry lost its id/epoch CAS");
            }
        }
        Ok(())
    }

    /// Atomically publish the operational owner of one held exclusive process
    /// and activate its already-constructed workspace. The caller may release
    /// the held child only after this transaction commits.
    pub fn attach_worker_process(&self, record: &WorkerProcessRecord) -> Result<()> {
        validate_worker_process_record(record)?;
        if record.state != WorkerProcessState::Attached || record.cleanup_state != "owned" {
            bail!("new worker process must enter as attached and owned");
        }
        let tx = self.conn.unchecked_transaction()?;
        let session: Option<(String, String, String, String, String, i64)> = tx
            .query_row(
                "SELECT admitted_capsule_hash, workspace_id, root_thread_id, state,
                        credential_profile_id, credential_generation
                 FROM dedicated_session WHERE session_id = ?1",
                [&record.session_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            capsule_hash,
            workspace_id,
            root_thread_id,
            session_state,
            profile_id,
            generation,
        )) = session
        else {
            bail!("worker process references an unknown dedicated session");
        };
        if capsule_hash != record.session_capsule_hash || session_state != "admitted" {
            bail!("worker process contradicts admitted session authority or state");
        }
        let credential_fenced: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM credential_profile
              WHERE profile_id=?1 AND credential_generation=?2 AND lock_owner=?3
                AND state IN ('unauthenticated','enrolling','confirming','active'))",
            params![profile_id, generation, record.worker_instance_id],
            |row| row.get(0),
        )?;
        if !credential_fenced {
            bail!("dedicated worker attachment lost its credential generation/lock fence");
        }
        let workspace: Option<(String, Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT execution_workspace.state,
                        execution_workspace.process_identity,
                        thread_runtime.process_identity
                   FROM execution_workspace
                   LEFT JOIN thread_runtime ON thread_runtime.thread_id = ?2
                  WHERE workspace_id = ?1 AND execution_workspace.thread_id = ?2",
                params![workspace_id, root_thread_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let workspace_handoff_identity = match workspace {
            Some((state, None, _)) if state == "ready" => None,
            Some((state, Some(workspace_identity), Some(root_identity)))
                if state == "active" && workspace_identity == root_identity =>
            {
                Some(workspace_identity)
            }
            _ => bail!(
                "dedicated worker workspace is not ready or actively owned by its root process"
            ),
        };
        let process_identity = serde_json::to_string(&record.process_identity)
            .context("serialize worker process identity")?;
        tx.execute(
            "INSERT INTO worker_process (
                worker_instance_id, boot_identity_hash, session_capsule_hash,
                boot_epoch, lifecycle_generation, process_identity,
                control_channel_identity, state, daemon_generation_id,
                session_id, cleanup_state, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                record.worker_instance_id,
                record.boot_identity_hash,
                record.session_capsule_hash,
                i64::try_from(record.boot_epoch)
                    .context("worker boot epoch exceeds SQLite range")?,
                i64::try_from(record.lifecycle_generation)
                    .context("worker lifecycle generation exceeds SQLite range")?,
                process_identity,
                record.control_channel_identity,
                record.state.as_str(),
                record.daemon_generation_id,
                record.session_id,
                record.cleanup_state,
                record.created_at_ms,
                record.updated_at_ms,
            ],
        )?;
        let workspace_changed = match workspace_handoff_identity {
            None => tx.execute(
                "UPDATE execution_workspace
                    SET state = 'active', process_identity = ?2, updated_at_ms = ?3
                  WHERE workspace_id = ?1 AND state = 'ready' AND process_identity IS NULL",
                params![workspace_id, process_identity, record.updated_at_ms],
            )?,
            Some(root_identity) => tx.execute(
                "UPDATE execution_workspace
                    SET process_identity = ?2, updated_at_ms = ?3
                  WHERE workspace_id = ?1 AND state = 'active' AND process_identity = ?4
                    AND EXISTS(SELECT 1 FROM thread_runtime
                      WHERE thread_id = ?5 AND process_identity = ?4)",
                params![
                    workspace_id,
                    process_identity,
                    record.updated_at_ms,
                    root_identity,
                    root_thread_id,
                ],
            )?,
        };
        let session_changed = tx.execute(
            "UPDATE dedicated_session
             SET worker_instance_id = ?2, worker_boot_epoch = ?3,
                 state = 'binding', updated_at_ms = ?4
             WHERE session_id = ?1 AND state = 'admitted' AND worker_instance_id IS NULL",
            params![
                record.session_id,
                record.worker_instance_id,
                i64::try_from(record.boot_epoch)
                    .context("worker boot epoch exceeds SQLite range")?,
                record.updated_at_ms,
            ],
        )?;
        if workspace_changed != 1 || session_changed != 1 {
            bail!("dedicated worker atomic attachment lost its workspace/session CAS");
        }
        tx.commit()?;
        Ok(())
    }

    /// Preserve exact process evidence when atomic attachment failed and the
    /// held child could not be proved reaped. This quarantine row lets
    /// revocation/restart verify the exact group before releasing credentials.
    pub fn fence_unproved_worker_start(
        &self,
        record: &WorkerProcessRecord,
        reason: &str,
    ) -> Result<()> {
        validate_worker_process_record(record)?;
        validate_bounded_runtime_text("unproved worker start reason", reason, 4096)?;
        let process_identity = serde_json::to_string(&record.process_identity)?;
        let epoch = i64::try_from(record.boot_epoch)?;
        let now = lillux::time::timestamp_millis() as i64;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO worker_process (
                worker_instance_id, boot_identity_hash, session_capsule_hash,
                boot_epoch, lifecycle_generation, process_identity,
                control_channel_identity, state, daemon_generation_id,
                session_id, cleanup_state, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'dead', ?8, ?9, 'unproved', ?10, ?10)
             ON CONFLICT(worker_instance_id) DO NOTHING",
            params![
                record.worker_instance_id,
                record.boot_identity_hash,
                record.session_capsule_hash,
                epoch,
                i64::try_from(record.lifecycle_generation)?,
                process_identity,
                record.control_channel_identity,
                record.daemon_generation_id,
                record.session_id,
                now,
            ],
        )?;
        let exact: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM worker_process
              WHERE worker_instance_id=?1 AND session_id=?2 AND boot_epoch=?3
                AND process_identity=?4 AND state='dead' AND cleanup_state='unproved')",
            params![
                record.worker_instance_id,
                record.session_id,
                epoch,
                serde_json::to_string(&record.process_identity)?,
            ],
            |row| row.get(0),
        )?;
        if !exact {
            bail!("unproved worker identity conflicts with existing durable evidence");
        }
        let changed = tx.execute(
            "UPDATE dedicated_session
                SET worker_instance_id=?2, worker_boot_epoch=?3,
                    state='outcome_unknown', send_boundary='outcome_unknown',
                    terminal_reason=?4, updated_at_ms=?5
              WHERE session_id=?1
                AND (worker_instance_id IS NULL OR worker_instance_id=?2)
                AND (worker_boot_epoch IS NULL OR worker_boot_epoch=?3)
                AND state IN ('admitted','binding','recovering','outcome_unknown')",
            params![
                record.session_id,
                record.worker_instance_id,
                epoch,
                reason,
                now
            ],
        )?;
        if changed != 1 {
            bail!("unproved worker start fence lost its session identity CAS");
        }
        tx.commit()?;
        Ok(())
    }

    pub fn worker_process(&self, worker_instance_id: &str) -> Result<Option<WorkerProcessRecord>> {
        let raw = self
            .conn
            .query_row(
                "SELECT worker_instance_id, boot_identity_hash, session_capsule_hash,
                        boot_epoch, lifecycle_generation, process_identity,
                        control_channel_identity, state, daemon_generation_id,
                        session_id, cleanup_state, created_at_ms, updated_at_ms
                 FROM worker_process WHERE worker_instance_id = ?1",
                [worker_instance_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, String>(10)?,
                        row.get::<_, i64>(11)?,
                        row.get::<_, i64>(12)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            instance,
            boot_hash,
            capsule,
            boot_epoch,
            lifecycle_generation,
            process_identity,
            control_channel,
            state,
            daemon_generation,
            session_id,
            cleanup_state,
            created_at_ms,
            updated_at_ms,
        )) = raw
        else {
            return Ok(None);
        };
        let record = WorkerProcessRecord {
            worker_instance_id: instance,
            boot_identity_hash: boot_hash,
            session_capsule_hash: capsule,
            boot_epoch: u64::try_from(boot_epoch).context("negative worker boot epoch")?,
            lifecycle_generation: u64::try_from(lifecycle_generation)
                .context("negative worker lifecycle generation")?,
            process_identity: serde_json::from_str(&process_identity)
                .context("decode worker process identity")?,
            control_channel_identity: control_channel,
            state: WorkerProcessState::parse(&state)?,
            daemon_generation_id: daemon_generation,
            session_id,
            cleanup_state,
            created_at_ms,
            updated_at_ms,
        };
        validate_worker_process_record(&record)?;
        Ok(Some(record))
    }

    pub fn live_worker_processes(&self) -> Result<Vec<WorkerProcessRecord>> {
        let mut statement = self.conn.prepare(
            "SELECT worker_instance_id FROM worker_process
              WHERE state IN ('starting','attached','live','draining')
                 OR (state='dead' AND cleanup_state='unproved')
              ORDER BY created_at_ms, worker_instance_id",
        )?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| {
                self.worker_process(&id)?
                    .ok_or_else(|| anyhow!("listed worker process disappeared"))
            })
            .collect()
    }

    /// Fence a previous daemon generation before any replacement worker can
    /// be admitted. Contacted commands become permanently ambiguous and
    /// pending approvals become stale; neither may be replayed into a new
    /// process epoch.
    pub fn fence_abandoned_worker_process(
        &self,
        worker_instance_id: &str,
        session_id: &str,
        boot_epoch: u64,
        cleanup_state: &str,
    ) -> Result<()> {
        if !matches!(cleanup_state, "reaped" | "unproved") {
            bail!("abandoned worker cleanup state must be reaped or unproved");
        }
        let epoch = i64::try_from(boot_epoch).context("worker boot epoch exceeds SQLite range")?;
        let now = lillux::time::timestamp_millis() as i64;
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE worker_process SET state='dead', cleanup_state=?4, updated_at_ms=?5
              WHERE worker_instance_id=?1 AND session_id=?2 AND boot_epoch=?3
                AND state IN ('starting','attached','live','draining')",
            params![worker_instance_id, session_id, epoch, cleanup_state, now],
        )?;
        if changed != 1 {
            bail!("abandoned worker fence lost its process-identity CAS");
        }
        tx.execute(
            "UPDATE dedicated_session_approval SET state='delivery_unknown', resolved_at_ms=?3
              WHERE session_id=?1 AND worker_boot_epoch=?2 AND state='delivery_contacting'",
            params![session_id, epoch, now],
        )?;
        tx.execute(
            "UPDATE dedicated_session_approval SET state='stale_epoch', resolved_at_ms=?3
              WHERE session_id=?1 AND worker_boot_epoch=?2
                AND state IN ('pending', 'decision_reserved')",
            params![session_id, epoch, now],
        )?;
        // `committed` is mechanically before possible worker contact: the
        // contacting transition is durable before the socket write. Retire
        // these old-epoch reservations as a stable retryable failure so they
        // cannot block recovery or a later command.
        tx.execute(
            "UPDATE dedicated_session_command
                SET state='failed', result_json=?3, updated_at_ms=?4
              WHERE session_id=?1 AND worker_boot_epoch=?2 AND state='committed'",
            params![
                session_id,
                epoch,
                serde_json::to_string(&serde_json::json!({
                    "error":"worker epoch ended before contact",
                    "retryable_uncontacted":true,
                }))?,
                now
            ],
        )?;
        let contacted = tx.execute(
            "UPDATE dedicated_session_command SET state='outcome_unknown', updated_at_ms=?3
              WHERE session_id=?1 AND worker_boot_epoch=?2 AND state='dispatched'",
            params![session_id, epoch, now],
        )?;
        let next_state = if contacted > 0 || cleanup_state == "unproved" {
            "outcome_unknown"
        } else {
            "recovering"
        };
        let next_boundary = if contacted > 0 || cleanup_state == "unproved" {
            "outcome_unknown"
        } else {
            "none"
        };
        let session_changed = if cleanup_state == "reaped" {
            tx.execute(
                "UPDATE dedicated_session
                    SET worker_instance_id=NULL, worker_boot_epoch=NULL, state=?4,
                        send_boundary=?5, current_turn_id=NULL, updated_at_ms=?6
                  WHERE session_id=?1 AND worker_instance_id=?2 AND worker_boot_epoch=?3
                    AND state NOT IN ('terminal','frozen','publish_ready')",
                params![
                    session_id,
                    worker_instance_id,
                    epoch,
                    next_state,
                    next_boundary,
                    now
                ],
            )?
        } else {
            // Unproved cleanup is still a live-credential possibility. Retain
            // the exact worker identity and profile fence so revocation and
            // recovery can never mistake it for a safely unattached session.
            tx.execute(
                "UPDATE dedicated_session
                    SET state='outcome_unknown', send_boundary='outcome_unknown', updated_at_ms=?4
                  WHERE session_id=?1 AND worker_instance_id=?2 AND worker_boot_epoch=?3
                    AND state NOT IN ('terminal','frozen','publish_ready')",
                params![session_id, worker_instance_id, epoch, now],
            )?
        };
        if session_changed != 1 {
            bail!("abandoned worker fence lost its session-epoch CAS");
        }
        if cleanup_state == "reaped" {
            tx.execute(
                "UPDATE execution_workspace SET state='ready', process_identity=NULL, updated_at_ms=?2
                  WHERE workspace_id=(SELECT workspace_id FROM dedicated_session WHERE session_id=?1)
                    AND state='active'",
                params![session_id, now],
            )?;
            tx.execute(
                "UPDATE credential_profile SET lock_owner=NULL, updated_at_ms=?3
                  WHERE profile_id=(SELECT credential_profile_id FROM dedicated_session
                                      WHERE session_id=?1)
                    AND lock_owner=?2",
                params![session_id, worker_instance_id, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Publish readiness only after the worker completed its signed boot
    /// handshake. Both the process and its owning session cross the boundary
    /// in one transaction, fenced by the boot epoch.
    pub fn complete_worker_binding(
        &self,
        worker_instance_id: &str,
        session_id: &str,
        boot_epoch: u64,
    ) -> Result<()> {
        validate_bounded_runtime_text("worker instance id", worker_instance_id, 256)?;
        validate_bounded_runtime_text("dedicated session id", session_id, 256)?;
        if boot_epoch == 0 {
            bail!("worker boot epoch must be positive");
        }
        let epoch = i64::try_from(boot_epoch).context("worker boot epoch exceeds SQLite range")?;
        let now = lillux::time::timestamp_millis() as i64;
        let tx = self.conn.unchecked_transaction()?;
        let worker_changed = tx.execute(
            "UPDATE worker_process SET state = 'live', updated_at_ms = ?4
             WHERE worker_instance_id = ?1 AND session_id = ?2 AND boot_epoch = ?3
               AND state = 'attached' AND cleanup_state = 'owned'
               AND EXISTS(SELECT 1 FROM dedicated_session
                 JOIN credential_profile ON credential_profile.profile_id=dedicated_session.credential_profile_id
                 WHERE dedicated_session.session_id=?2
                   AND dedicated_session.worker_instance_id=?1
                   AND dedicated_session.worker_boot_epoch=?3
                   AND credential_profile.credential_generation=dedicated_session.credential_generation
                   AND credential_profile.lock_owner=?1
                   AND credential_profile.state IN ('unauthenticated','enrolling','confirming','active'))",
            params![worker_instance_id, session_id, epoch, now],
        )?;
        let session_changed = tx.execute(
            "UPDATE dedicated_session
                SET state = CASE WHEN remote_thread_id IS NULL THEN 'idle' ELSE 'recovering' END,
                    send_boundary = 'none',
                    updated_at_ms = ?4
             WHERE session_id = ?1 AND worker_instance_id = ?2
               AND worker_boot_epoch = ?3 AND state = 'binding'
               AND EXISTS(SELECT 1 FROM credential_profile
                 WHERE profile_id=dedicated_session.credential_profile_id
                   AND credential_generation=dedicated_session.credential_generation
                   AND lock_owner=?2
                   AND state IN ('unauthenticated','enrolling','confirming','active'))",
            params![session_id, worker_instance_id, epoch, now],
        )?;
        if worker_changed != 1 || session_changed != 1 {
            bail!("worker readiness lost its attached process/session epoch");
        }
        tx.commit()?;
        Ok(())
    }

    /// Fence and settle an owned worker after exact process-group cleanup.
    pub fn settle_worker_process(
        &self,
        worker_instance_id: &str,
        session_id: &str,
        boot_epoch: u64,
        cleanup_state: &str,
        terminal_reason: &str,
    ) -> Result<()> {
        validate_bounded_runtime_text("worker instance id", worker_instance_id, 256)?;
        validate_bounded_runtime_text("dedicated session id", session_id, 256)?;
        validate_bounded_runtime_text("worker cleanup state", cleanup_state, 32)?;
        validate_bounded_runtime_text("worker terminal reason", terminal_reason, 2048)?;
        if !matches!(cleanup_state, "reaped" | "unproved") || boot_epoch == 0 {
            bail!("worker settlement is not canonical");
        }
        let epoch = i64::try_from(boot_epoch).context("worker boot epoch exceeds SQLite range")?;
        let now = lillux::time::timestamp_millis() as i64;
        let tx = self.conn.unchecked_transaction()?;
        let worker_changed = tx.execute(
            "UPDATE worker_process SET state = 'dead', cleanup_state = ?4,
                    updated_at_ms = ?5
             WHERE worker_instance_id = ?1 AND session_id = ?2 AND boot_epoch = ?3
               AND (state IN ('attached', 'live', 'draining')
                    OR (state='dead' AND cleanup_state='unproved' AND ?4='reaped'))",
            params![worker_instance_id, session_id, epoch, cleanup_state, now],
        )?;
        let session_changed = tx.execute(
            "UPDATE dedicated_session SET state = 'recovering', terminal_reason = ?4,
                    updated_at_ms = ?5
             WHERE session_id = ?1 AND worker_instance_id = ?2
               AND worker_boot_epoch = ?3
               AND state IN ('admitted','binding','idle','turn_running','awaiting_approval',
                             'recovering','outcome_unknown','draining')",
            params![session_id, worker_instance_id, epoch, terminal_reason, now],
        )?;
        if worker_changed != 1 {
            let retry: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM worker_process
                  WHERE worker_instance_id=?1 AND session_id=?2 AND boot_epoch=?3
                    AND state='dead' AND cleanup_state=?4)",
                params![worker_instance_id, session_id, epoch, cleanup_state],
                |row| row.get(0),
            )?;
            if !retry {
                bail!("worker settlement lost its process identity");
            }
        }
        if session_changed != 1 {
            let retry: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM dedicated_session
                  WHERE session_id=?1 AND worker_instance_id=?2 AND worker_boot_epoch=?3
                    AND state IN ('recovering','freezing','frozen','verifying','publish_ready',
                                  'publishing','discarding','terminal'))",
                params![session_id, worker_instance_id, epoch],
                |row| row.get(0),
            )?;
            if !retry {
                bail!("worker settlement lost its session epoch");
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn terminalize_dedicated_session(
        &self,
        session_id: &str,
        worker_instance_id: &str,
        boot_epoch: u64,
        reason: &str,
    ) -> Result<()> {
        validate_bounded_runtime_text("dedicated session id", session_id, 256)?;
        validate_bounded_runtime_text("worker instance id", worker_instance_id, 256)?;
        validate_bounded_runtime_text("dedicated terminal reason", reason, 2048)?;
        let now = lillux::time::timestamp_millis() as i64;
        let tx = self.conn.unchecked_transaction()?;
        let candidate_required: bool = tx.query_row(
            "SELECT candidate_required != 0 FROM dedicated_session
              WHERE session_id=?1 AND worker_instance_id=?2 AND worker_boot_epoch=?3",
            params![session_id, worker_instance_id, i64::try_from(boot_epoch)?],
            |row| row.get(0),
        )?;
        let terminal_state = if reason == "completed" && candidate_required {
            "freezing"
        } else {
            "terminal"
        };
        let session_changed = tx.execute(
            "UPDATE dedicated_session SET state=?6, terminal_reason=?4,
                    current_turn_id=NULL, updated_at_ms=?5
              WHERE session_id=?1 AND worker_instance_id=?2 AND worker_boot_epoch=?3
                AND state='recovering'
                AND EXISTS(SELECT 1 FROM worker_process
                    WHERE worker_instance_id=?2 AND session_id=?1 AND boot_epoch=?3
                      AND state='dead' AND cleanup_state='reaped')",
            params![
                session_id,
                worker_instance_id,
                i64::try_from(boot_epoch)?,
                reason,
                now,
                terminal_state,
            ],
        )?;
        if session_changed != 1 {
            let retry: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM dedicated_session
                  WHERE session_id=?1 AND worker_instance_id=?2 AND worker_boot_epoch=?3
                    AND terminal_reason=?4 AND state=?5)",
                params![
                    session_id,
                    worker_instance_id,
                    i64::try_from(boot_epoch)?,
                    reason,
                    terminal_state
                ],
                |row| row.get(0),
            )?;
            if !retry {
                bail!("dedicated terminal settlement lost its worker/session CAS");
            }
        }
        tx.execute(
            "UPDATE dedicated_session_approval SET state='delivery_unknown', resolved_at_ms=?2
              WHERE session_id=?1 AND state='delivery_contacting'",
            params![session_id, now],
        )?;
        tx.execute(
            "UPDATE dedicated_session_approval SET state='stale_epoch', resolved_at_ms=?2
              WHERE session_id=?1 AND state IN ('pending', 'decision_reserved')",
            params![session_id, now],
        )?;
        // Terminal settlement owns the credential lease release in the same
        // durable transaction. This closes the crash window where a dead
        // worker had terminalized its session but left the profile fenced.
        tx.execute(
            "UPDATE credential_profile
                SET state=CASE WHEN state='enrolling' THEN 'unauthenticated' ELSE state END,
                    active_login_id=CASE WHEN state='enrolling' THEN NULL ELSE active_login_id END,
                    login_expires_at_ms=CASE WHEN state='enrolling' THEN NULL ELSE login_expires_at_ms END,
                    lock_owner=NULL, updated_at_ms=?4
              WHERE profile_id=(SELECT credential_profile_id FROM dedicated_session
                                  WHERE session_id=?1)
                AND lock_owner=?2
                AND EXISTS(SELECT 1 FROM dedicated_session
                  WHERE session_id=?1 AND worker_boot_epoch=?3)",
            params![session_id, worker_instance_id, i64::try_from(boot_epoch)?, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn bind_dedicated_session_candidate(
        &self,
        root_thread_id: &str,
        snapshot_hash: &str,
    ) -> Result<bool> {
        validate_bounded_runtime_text("dedicated root thread id", root_thread_id, 256)?;
        if !lillux::valid_hash(snapshot_hash) {
            bail!("dedicated candidate snapshot hash is not canonical");
        }
        let candidate_validation_hash =
            ryeos_state::objects::canonical_value_digest(&serde_json::json!({
                "schema":"ryeos.dedicated_candidate_verification.v1",
                "candidate_snapshot_hash":snapshot_hash,
                "checks":["canonical_snapshot_manifest","base_ancestry_at_publication"]
            }))?;
        let changed = self.conn.execute(
            "UPDATE dedicated_session
                SET candidate_snapshot_hash=?2, candidate_validation_hash=?3,
                    publication_result='retained', state='frozen', updated_at_ms=?4
              WHERE root_thread_id=?1 AND state='freezing'
                AND terminal_reason='completed'
                AND candidate_required=1
                AND candidate_snapshot_hash IS NULL
                AND EXISTS(SELECT 1 FROM execution_workspace
                    WHERE workspace_id=dedicated_session.workspace_id AND state='closed')",
            params![
                root_thread_id,
                snapshot_hash,
                candidate_validation_hash,
                lillux::time::timestamp_millis() as i64
            ],
        )?;
        Ok(changed == 1)
    }

    pub fn reserve_dedicated_candidate_validation(
        &self,
        session_id: &str,
        candidate_snapshot_hash: &str,
        candidate_validation_hash: &str,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE dedicated_session SET state='verifying', disposition_resume_state='frozen', updated_at_ms=?4
              WHERE session_id=?1 AND state='frozen'
                AND candidate_snapshot_hash=?2 AND candidate_validation_hash=?3
                AND publication_result='retained'",
            params![
                session_id,
                candidate_snapshot_hash,
                candidate_validation_hash,
                lillux::time::timestamp_millis() as i64
            ],
        )?;
        if changed == 1 {
            return Ok(true);
        }
        let retry: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM dedicated_session
              WHERE session_id=?1 AND state='verifying'
                AND candidate_snapshot_hash=?2 AND candidate_validation_hash=?3
                AND publication_result='retained')",
            params![
                session_id,
                candidate_snapshot_hash,
                candidate_validation_hash
            ],
            |row| row.get(0),
        )?;
        if !retry {
            bail!("dedicated candidate validation lost its identity/state CAS");
        }
        Ok(false)
    }

    pub fn settle_dedicated_candidate_validation(
        &self,
        session_id: &str,
        candidate_snapshot_hash: &str,
        candidate_validation_hash: &str,
        evidence: &Value,
    ) -> Result<()> {
        validate_bounded_runtime_text("dedicated session id", session_id, 256)?;
        if !lillux::valid_hash(candidate_snapshot_hash)
            || !lillux::valid_hash(candidate_validation_hash)
        {
            bail!("dedicated validation identity is not canonical");
        }
        let _evidence_digest = ryeos_state::objects::canonical_value_digest(evidence)?;
        let now = lillux::time::timestamp_millis() as i64;
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE dedicated_session SET state='publish_ready', disposition_resume_state=NULL, updated_at_ms=?4
              WHERE session_id=?1 AND state='verifying'
                AND candidate_snapshot_hash=?2 AND candidate_validation_hash=?3
                AND publication_result='retained'",
            params![
                session_id,
                candidate_snapshot_hash,
                candidate_validation_hash,
                now
            ],
        )?;
        if changed != 1 {
            bail!("dedicated candidate validation lost its identity/state CAS");
        }
        tx.commit()?;
        Ok(())
    }

    pub fn reserve_dedicated_candidate_discard(
        &self,
        session_id: &str,
        candidate_snapshot_hash: &str,
    ) -> Result<()> {
        validate_bounded_runtime_text("dedicated session id", session_id, 256)?;
        if !lillux::valid_hash(candidate_snapshot_hash) {
            bail!("discarded candidate hash is not canonical");
        }
        let changed = self.conn.execute(
            "UPDATE dedicated_session
                SET disposition_resume_state=state, state='discarding', updated_at_ms=?3
              WHERE session_id=?1 AND candidate_snapshot_hash=?2
                AND publication_result='retained' AND state IN ('frozen','publish_ready')",
            params![
                session_id,
                candidate_snapshot_hash,
                lillux::time::timestamp_millis() as i64
            ],
        )?;
        if changed != 1 {
            let retry: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM dedicated_session
                  WHERE session_id=?1 AND candidate_snapshot_hash=?2
                    AND publication_result='retained' AND state='discarding')",
                params![session_id, candidate_snapshot_hash],
                |row| row.get(0),
            )?;
            if !retry {
                bail!("dedicated candidate discard lost its identity/state CAS");
            }
        }
        Ok(())
    }

    pub fn settle_dedicated_candidate_discard(
        &self,
        session_id: &str,
        candidate_snapshot_hash: &str,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE dedicated_session
                SET state='terminal', publication_result='discarded',
                    disposition_resume_state=NULL, updated_at_ms=?3
              WHERE session_id=?1 AND candidate_snapshot_hash=?2
                AND publication_result='retained' AND state='discarding'",
            params![
                session_id,
                candidate_snapshot_hash,
                lillux::time::timestamp_millis() as i64
            ],
        )?;
        if changed != 1 {
            bail!("dedicated candidate discard settlement lost its reservation");
        }
        Ok(())
    }

    pub fn reserve_dedicated_candidate_publication(
        &self,
        session_id: &str,
        candidate_snapshot_hash: &str,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE dedicated_session SET state='publishing', disposition_resume_state='publish_ready', updated_at_ms=?3
              WHERE session_id=?1 AND candidate_snapshot_hash=?2
                AND publication_result='retained' AND state='publish_ready'",
            params![
                session_id,
                candidate_snapshot_hash,
                lillux::time::timestamp_millis() as i64
            ],
        )?;
        if changed != 1 {
            let retry: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM dedicated_session
                  WHERE session_id=?1 AND candidate_snapshot_hash=?2
                    AND publication_result='retained' AND state='publishing')",
                params![session_id, candidate_snapshot_hash],
                |row| row.get(0),
            )?;
            if !retry {
                bail!("dedicated candidate publication lost its identity/state CAS");
            }
        }
        Ok(())
    }

    pub fn settle_dedicated_candidate_publication(
        &self,
        session_id: &str,
        candidate_snapshot_hash: &str,
        publication_result: &str,
    ) -> Result<()> {
        validate_bounded_runtime_text("dedicated session id", session_id, 256)?;
        if !lillux::valid_hash(candidate_snapshot_hash) {
            bail!("published candidate snapshot hash is not canonical");
        }
        validate_bounded_runtime_text("dedicated publication result", publication_result, 512)?;
        let changed = self.conn.execute(
            "UPDATE dedicated_session SET publication_result=?3, state='terminal',
                    disposition_resume_state=NULL, updated_at_ms=?4
              WHERE session_id=?1 AND state='publishing'
                AND candidate_snapshot_hash=?2
                AND publication_result='retained'",
            params![
                session_id,
                candidate_snapshot_hash,
                publication_result,
                lillux::time::timestamp_millis() as i64
            ],
        )?;
        if changed != 1 {
            bail!("dedicated publication result lost its session/candidate CAS");
        }
        Ok(())
    }

    pub fn fail_dedicated_candidate_disposition(
        &self,
        session_id: &str,
        reserved_state: &str,
    ) -> Result<()> {
        if !matches!(reserved_state, "verifying" | "publishing" | "discarding") {
            bail!("candidate disposition failure state is invalid");
        }
        let fallback = match reserved_state {
            "verifying" => "frozen",
            "publishing" => "publish_ready",
            "discarding" => "", // retained per-row below
            _ => unreachable!(),
        };
        let changed = if reserved_state == "discarding" {
            self.conn.execute(
                "UPDATE dedicated_session SET state=disposition_resume_state,
                    disposition_resume_state=NULL, updated_at_ms=?3
                  WHERE session_id=?1 AND state=?2
                    AND disposition_resume_state IN ('frozen','publish_ready')
                    AND publication_result='retained'",
                params![
                    session_id,
                    reserved_state,
                    lillux::time::timestamp_millis() as i64
                ],
            )?
        } else {
            self.conn.execute(
                "UPDATE dedicated_session SET state=?3, disposition_resume_state=NULL,
                    updated_at_ms=?4
                  WHERE session_id=?1 AND state=?2 AND publication_result='retained'",
                params![
                    session_id,
                    reserved_state,
                    fallback,
                    lillux::time::timestamp_millis() as i64
                ],
            )?
        };
        if changed != 1 {
            bail!("candidate disposition failure lost its reservation");
        }
        Ok(())
    }

    pub fn cancel_dedicated_candidate_for_root_stop(&self, session_id: &str) -> Result<()> {
        let now = lillux::time::timestamp_millis() as i64;
        let changed = self.conn.execute(
            "UPDATE dedicated_session
                SET state='terminal', terminal_reason='cancelled',
                    publication_result=CASE
                      WHEN candidate_snapshot_hash IS NULL THEN 'abandoned'
                      ELSE 'discarded'
                    END,
                    disposition_resume_state=NULL, updated_at_ms=?2
              WHERE session_id=?1
                AND state IN ('freezing','frozen','verifying','publish_ready','discarding')
                AND EXISTS(SELECT 1 FROM worker_process
                  WHERE worker_instance_id=dedicated_session.worker_instance_id
                    AND session_id=dedicated_session.session_id
                    AND boot_epoch=dedicated_session.worker_boot_epoch
                    AND state='dead' AND cleanup_state='reaped')",
            params![session_id, now],
        )?;
        if changed != 1 {
            bail!("candidate root-stop cancellation lost its dead-worker/session proof");
        }
        Ok(())
    }

    pub fn reserve_launch_planning(
        &self,
        launch_id: &str,
        reserved_thread_id: &str,
        requested_by: &str,
    ) -> Result<()> {
        const MAX_PENDING_ROWS: i64 = 4_096;
        self.reserve_launch_planning_bounded(
            launch_id,
            reserved_thread_id,
            requested_by,
            MAX_PENDING_ROWS,
        )
    }

    fn reserve_launch_planning_bounded(
        &self,
        launch_id: &str,
        reserved_thread_id: &str,
        requested_by: &str,
        max_pending_rows: i64,
    ) -> Result<()> {
        if max_pending_rows < 1 {
            bail!("launch planning pending-row capacity must be positive");
        }
        let now = lillux::time::timestamp_millis() as i64;
        let tx = self.conn.unchecked_transaction()?;
        let already_reserved: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM launch_planning WHERE launch_id = ?1)",
            [launch_id],
            |row| row.get(0),
        )?;
        if already_reserved {
            return Err(LaunchPlanningAlreadyReserved.into());
        }
        let pending_rows: i64 = tx.query_row(
            "SELECT COUNT(*) FROM launch_planning WHERE state = 'planning'",
            [],
            |row| row.get(0),
        )?;
        if pending_rows >= max_pending_rows {
            return Err(LaunchPlanningCapacityExceeded.into());
        }
        tx.execute(
            "INSERT INTO launch_planning (
                launch_id, reserved_thread_id, requested_by, daemon_generation_id,
                state, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, 'planning', ?5, ?5)",
            params![
                launch_id,
                reserved_thread_id,
                requested_by,
                daemon_generation_id(),
                now
            ],
        )?;
        prune_launch_planning(&tx, now)?;
        tx.commit()?;
        Ok(())
    }

    pub fn launch_planning_by_id(&self, launch_id: &str) -> Result<Option<LaunchPlanningRecord>> {
        read_launch_planning(
            &self.conn,
            "SELECT launch_id, reserved_thread_id, requested_by, daemon_generation_id,
                    state, bound_thread_id, outcome_code
               FROM launch_planning WHERE launch_id = ?1",
            launch_id,
        )
    }

    pub fn launch_planning_by_thread(
        &self,
        thread_id: &str,
    ) -> Result<Option<LaunchPlanningRecord>> {
        read_launch_planning(
            &self.conn,
            "SELECT launch_id, reserved_thread_id, requested_by, daemon_generation_id,
                    state, bound_thread_id, outcome_code
               FROM launch_planning WHERE reserved_thread_id = ?1",
            thread_id,
        )
    }

    pub fn pending_launch_planning(&self) -> Result<Vec<LaunchPlanningRecord>> {
        let mut statement = self.conn.prepare(
            "SELECT launch_id, reserved_thread_id, requested_by, daemon_generation_id,
                    state, bound_thread_id, outcome_code
               FROM launch_planning WHERE state = 'planning'
               ORDER BY created_at_ms, launch_id",
        )?;
        let records = statement
            .query_map([], |row| {
                Ok(LaunchPlanningRecord {
                    launch_id: row.get(0)?,
                    reserved_thread_id: row.get(1)?,
                    requested_by: row.get(2)?,
                    daemon_generation_id: row.get(3)?,
                    state: row.get(4)?,
                    bound_thread_id: row.get(5)?,
                    outcome_code: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(records)
    }

    pub fn dedicated_approval_outbox_session_ids(&self) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare(
            "SELECT DISTINCT session_id FROM dedicated_session_approval
              WHERE state IN ('decision_reserved','delivery_contacting','delivery_unknown')
              ORDER BY session_id",
        )?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn cancel_unbound_launch_planning(&self, launch_id: &str) -> Result<bool> {
        let now = lillux::time::timestamp_millis() as i64;
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE launch_planning
                SET state = 'cancelled', outcome_code = 'cancelled_by_requester',
                    updated_at_ms = ?2, finished_at_ms = ?2
              WHERE launch_id = ?1 AND state = 'planning'",
            params![launch_id, now],
        )? == 1;
        prune_launch_planning(&tx, now)?;
        tx.commit()?;
        Ok(changed)
    }

    pub fn bind_launch_planning(&self, reserved_thread_id: &str) -> Result<bool> {
        let now = lillux::time::timestamp_millis() as i64;
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE launch_planning
                SET state = 'bound', bound_thread_id = reserved_thread_id,
                    outcome_code = 'thread_bound', updated_at_ms = ?2, finished_at_ms = ?2
              WHERE reserved_thread_id = ?1 AND state = 'planning'",
            params![reserved_thread_id, now],
        )? == 1;
        prune_launch_planning(&tx, now)?;
        tx.commit()?;
        Ok(changed)
    }

    pub fn fail_launch_planning(&self, reserved_thread_id: &str) -> Result<bool> {
        self.fail_launch_planning_with_outcome(reserved_thread_id, "thread_creation_failed")
    }

    pub fn fail_launch_planning_admission(&self, reserved_thread_id: &str) -> Result<bool> {
        self.fail_launch_planning_with_outcome(reserved_thread_id, "launch_admission_failed")
    }

    fn fail_launch_planning_with_outcome(
        &self,
        reserved_thread_id: &str,
        outcome_code: &str,
    ) -> Result<bool> {
        let now = lillux::time::timestamp_millis() as i64;
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE launch_planning
                SET state = 'failed', outcome_code = ?2,
                    updated_at_ms = ?3, finished_at_ms = ?3
              WHERE reserved_thread_id = ?1 AND state = 'planning'",
            params![reserved_thread_id, outcome_code, now],
        )? == 1;
        prune_launch_planning(&tx, now)?;
        tx.commit()?;
        Ok(changed)
    }

    pub fn expire_stale_launch_planning(&self) -> Result<usize> {
        let now = lillux::time::timestamp_millis() as i64;
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE launch_planning
                SET state = 'expired', outcome_code = 'daemon_restarted_before_thread_bind',
                    updated_at_ms = ?2, finished_at_ms = ?2
              WHERE state = 'planning' AND daemon_generation_id <> ?1",
            params![daemon_generation_id(), now],
        )?;
        prune_launch_planning(&tx, now)?;
        tx.commit()?;
        Ok(changed)
    }

    /// Clear the complete daemon-owned execution runtime store for an
    /// explicitly authorized offline all-thread-history discard. The database
    /// and its exact current schema remain in place; node configuration and
    /// every non-thread store live elsewhere.
    pub fn discard_all_thread_history(
        &self,
        dry_run: bool,
    ) -> Result<RuntimeThreadHistoryDiscardReport> {
        fn count(conn: &Connection, table: &str) -> Result<usize> {
            let rows: i64 =
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })?;
            usize::try_from(rows).context("runtime thread-history row count is invalid")
        }

        if dry_run {
            return Ok(RuntimeThreadHistoryDiscardReport {
                thread_runtime: count(&self.conn, "thread_runtime")?,
                in_process_handler_reservations: count(
                    &self.conn,
                    "in_process_handler_reservation",
                )?,
                thread_commands: count(&self.conn, "thread_commands")?,
                hook_dispatch_ledger: count(&self.conn, "hook_dispatch_ledger")?,
                detached_spawn_intents: count(&self.conn, "detached_spawn_intent")?,
                recovery_waits: count(&self.conn, "thread_recovery_wait")?,
                thread_launch_claims: count(&self.conn, "thread_launch_claim")?,
                thread_launch_epochs: count(&self.conn, "thread_launch_epoch")?,
                execution_workspaces: count(&self.conn, "execution_workspace")?,
                follow_waiters: count(&self.conn, "follow_waiter")?,
                follow_waiter_children: count(&self.conn, "follow_waiter_child")?,
                thread_child_links: count(&self.conn, "thread_child_link")?,
                launch_windows: count(&self.conn, "launch_window")?,
                seat_leases: count(&self.conn, "seat_lease")?,
                launch_planning: count(&self.conn, "launch_planning")?,
            });
        }

        let conn = self.conn.unchecked_transaction()?;
        let report = RuntimeThreadHistoryDiscardReport {
            in_process_handler_reservations: conn
                .execute("DELETE FROM in_process_handler_reservation", [])?,
            follow_waiter_children: conn.execute("DELETE FROM follow_waiter_child", [])?,
            follow_waiters: conn.execute("DELETE FROM follow_waiter", [])?,
            thread_commands: conn.execute("DELETE FROM thread_commands", [])?,
            thread_launch_claims: conn.execute("DELETE FROM thread_launch_claim", [])?,
            thread_launch_epochs: conn.execute("DELETE FROM thread_launch_epoch", [])?,
            execution_workspaces: conn.execute("DELETE FROM execution_workspace", [])?,
            thread_child_links: conn.execute("DELETE FROM thread_child_link", [])?,
            launch_windows: conn.execute("DELETE FROM launch_window", [])?,
            seat_leases: conn.execute("DELETE FROM seat_lease", [])?,
            launch_planning: conn.execute("DELETE FROM launch_planning", [])?,
            hook_dispatch_ledger: conn.execute("DELETE FROM hook_dispatch_ledger", [])?,
            detached_spawn_intents: conn.execute("DELETE FROM detached_spawn_intent", [])?,
            recovery_waits: conn.execute("DELETE FROM thread_recovery_wait", [])?,
            thread_runtime: conn.execute("DELETE FROM thread_runtime", [])?,
        };
        conn.execute(
            "DELETE FROM sqlite_sequence WHERE name = 'thread_commands'",
            [],
        )?;
        conn.commit()?;
        Ok(report)
    }

    /// Atomically reserve the only permitted execution of a hook occurrence.
    /// A durable pending row is deliberately never reclaimed: after a crash
    /// the daemon cannot prove whether the external action ran, so re-dispatch
    /// would violate the at-most-once contract.
    pub fn reserve_hook_dispatch(&self, seed: &NewHookDispatch) -> Result<HookDispatchReservation> {
        validate_new_hook_dispatch(seed)?;
        let now = lillux::time::timestamp_millis() as i64;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO hook_dispatch_ledger(
                dispatch_key, seed_version, chain_root_id, caller_thread_id, event, hook_id,
                request_hash, status, created_at_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                seed.dispatch_key,
                seed.seed_version,
                seed.chain_root_id,
                seed.caller_thread_id,
                seed.event,
                seed.hook_id,
                seed.request_hash,
                HookDispatchStatus::Pending.as_str(),
                now,
            ],
        )?;
        if inserted == 1 {
            tx.commit()?;
            return Ok(HookDispatchReservation::Execute);
        }

        let (
            seed_version,
            chain_root_id,
            occurrence_thread_id,
            event,
            hook_id,
            request_hash,
            status,
            response_json,
            response_hash,
        ): (
            u32,
            String,
            String,
            String,
            String,
            String,
            String,
            Option<Vec<u8>>,
            Option<String>,
        ) = tx.query_row(
            "SELECT seed_version, chain_root_id, caller_thread_id, event, hook_id,
                    request_hash, status, response_json, response_hash
               FROM hook_dispatch_ledger WHERE dispatch_key=?1",
            params![seed.dispatch_key],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            },
        )?;
        if seed_version != seed.seed_version {
            bail!(
                "hook dispatch key `{}` belongs to seed version {seed_version}, expected {}",
                seed.dispatch_key,
                seed.seed_version
            );
        }
        if request_hash != seed.request_hash {
            bail!(
                "hook dispatch key `{}` was reused with a different request hash",
                seed.dispatch_key
            );
        }
        if chain_root_id != seed.chain_root_id || event != seed.event || hook_id != seed.hook_id {
            bail!(
                "hook dispatch key `{}` has divergent stored identity columns",
                seed.dispatch_key
            );
        }

        let reservation = match HookDispatchStatus::parse(&status)? {
            HookDispatchStatus::Pending => {
                if response_json.is_some() || response_hash.is_some() {
                    bail!(
                        "pending hook dispatch `{}` contains terminal response fields",
                        seed.dispatch_key
                    );
                }
                HookDispatchReservation::PendingUnknown
            }
            HookDispatchStatus::Completed => {
                HookDispatchReservation::Replay(decode_completed_hook_dispatch(
                    &seed.dispatch_key,
                    chain_root_id,
                    occurrence_thread_id,
                    event,
                    hook_id,
                    request_hash,
                    response_json.as_deref(),
                    response_hash.as_deref(),
                )?)
            }
        };
        tx.commit()?;
        Ok(reservation)
    }

    /// Bind one logical detached action to exactly one child identity. The
    /// binding precedes every workspace freeze and child-row mutation, so a
    /// callback retry after any crash boundary reuses the same identity.
    pub fn reserve_detached_spawn_intent(
        &self,
        operation_id: &str,
        parent_thread_id: &str,
        request_hash: &str,
        proposed_child_thread_id: &str,
        child_project_authority: Option<&ryeos_state::objects::ExecutionProjectAuthority>,
    ) -> Result<String> {
        validate_detached_spawn_intent(
            operation_id,
            parent_thread_id,
            request_hash,
            proposed_child_thread_id,
        )?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO detached_spawn_intent(
                operation_id, parent_thread_id, request_hash, child_thread_id,
                child_project_authority, created_at_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(operation_id) DO NOTHING",
            params![
                operation_id,
                parent_thread_id,
                request_hash,
                proposed_child_thread_id,
                child_project_authority
                    .map(encode_current_project_authority)
                    .transpose()?,
                lillux::time::timestamp_millis(),
            ],
        )?;
        let (stored_parent, stored_request, child_thread_id, stored_authority): (
            String,
            String,
            String,
            Option<String>,
        ) = tx.query_row(
            "SELECT parent_thread_id, request_hash, child_thread_id, child_project_authority
                   FROM detached_spawn_intent WHERE operation_id=?1",
            params![operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        if stored_parent != parent_thread_id || stored_request != request_hash {
            bail!(
                "detached operation `{operation_id}` was reused with different parent or request authority"
            );
        }
        let stored_authority = stored_authority
            .as_deref()
            .map(decode_current_project_authority)
            .transpose()?;
        if child_project_authority.is_some() && stored_authority.as_ref() != child_project_authority
        {
            bail!(
                "detached operation `{operation_id}` was reused with different project authority"
            );
        }
        tx.commit()?;
        Ok(child_thread_id)
    }

    /// Bind the project authority selected for a previously-reserved detached
    /// operation. Reservation and authority selection are deliberately two
    /// phases: the stable child identity is durable before an explicit project
    /// capture starts, while the capture itself is not authoritative until its
    /// exact snapshot authority is sealed here. Concurrent retries may bind the
    /// same value; a different value is an authority conflict.
    pub fn bind_detached_spawn_project_authority(
        &self,
        operation_id: &str,
        child_project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
    ) -> Result<()> {
        let encoded = encode_current_project_authority(child_project_authority)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE detached_spawn_intent
                SET child_project_authority=?2
              WHERE operation_id=?1 AND child_project_authority IS NULL",
            params![operation_id, encoded],
        )?;
        let stored: Option<String> = tx
            .query_row(
                "SELECT child_project_authority FROM detached_spawn_intent WHERE operation_id=?1",
                params![operation_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("detached operation `{operation_id}` is not reserved"))?;
        let stored: ryeos_state::objects::ExecutionProjectAuthority = stored
            .as_deref()
            .map(decode_current_project_authority)
            .transpose()?
            .ok_or_else(|| {
                anyhow!("detached operation `{operation_id}` project authority was not bound")
            })?;
        if stored != *child_project_authority {
            bail!("detached operation `{operation_id}` was bound to different project authority");
        }
        debug_assert!(changed <= 1);
        tx.commit()?;
        Ok(())
    }

    pub fn seal_detached_spawn_intent(
        &self,
        operation_id: &str,
        child_project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
        admitted_launch_capsule_hash: &str,
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        initial_events: &[crate::state_store::NewEventRecord],
    ) -> Result<()> {
        child_project_authority.validate()?;
        launch_metadata.validate()?;
        validate_sha256(
            "detached admitted_launch_capsule_hash",
            admitted_launch_capsule_hash,
        )?;
        let expected_capsule_hash = launch_metadata
            .admitted_launch_capsule()?
            .ok_or_else(|| anyhow!("detached sealed launch has no admitted capsule"))?
            .content_hash()?;
        if admitted_launch_capsule_hash != expected_capsule_hash {
            bail!(
                "detached admitted capsule hash mismatch: expected {expected_capsule_hash}, got {admitted_launch_capsule_hash}"
            );
        }
        let authority = encode_current_project_authority(child_project_authority)?;
        let metadata = encode_current_launch_metadata(launch_metadata)?;
        let events = serde_json::to_string(initial_events)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE detached_spawn_intent
             SET child_project_authority=?2, admitted_launch_capsule_hash=?3,
                 launch_metadata=?4, initial_events=?5
             WHERE operation_id=?1 AND launch_metadata IS NULL",
            params![
                operation_id,
                authority,
                admitted_launch_capsule_hash,
                metadata,
                events
            ],
        )?;
        let persisted = tx
            .query_row(
                "SELECT operation_id, parent_thread_id, request_hash, child_thread_id,
                        child_project_authority, admitted_launch_capsule_hash,
                        launch_metadata, initial_events
                 FROM detached_spawn_intent WHERE operation_id=?1",
                params![operation_id],
                decode_detached_spawn_intent,
            )
            .optional()?
            .ok_or_else(|| anyhow!("detached operation `{operation_id}` is not reserved"))?;
        if persisted
            .child_project_authority
            .as_ref()
            .map(encode_current_project_authority)
            .transpose()?
            .as_deref()
            != Some(authority.as_str())
            || persisted.admitted_launch_capsule_hash.as_deref()
                != Some(admitted_launch_capsule_hash)
            || persisted
                .launch_metadata
                .as_ref()
                .map(encode_current_launch_metadata)
                .transpose()?
                .as_deref()
                != Some(metadata.as_str())
            || persisted
                .initial_events
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?
                .as_deref()
                != Some(events.as_str())
        {
            bail!("detached operation `{operation_id}` was sealed with different launch authority");
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_detached_spawn_intent(
        &self,
        operation_id: &str,
    ) -> Result<Option<DetachedSpawnIntent>> {
        let row = self
            .conn
            .query_row(
                "SELECT operation_id, parent_thread_id, request_hash, child_thread_id,
                        child_project_authority, admitted_launch_capsule_hash,
                        launch_metadata, initial_events
                   FROM detached_spawn_intent WHERE operation_id=?1",
                params![operation_id],
                decode_detached_spawn_intent,
            )
            .optional()?;
        if let Some(intent) = &row {
            validate_detached_spawn_intent_record(intent)?;
        }
        Ok(row)
    }

    pub fn detached_spawn_intents(&self) -> Result<Vec<DetachedSpawnIntent>> {
        let mut statement = self.conn.prepare(
            "SELECT operation_id, parent_thread_id, request_hash, child_thread_id,
                    child_project_authority, admitted_launch_capsule_hash,
                    launch_metadata, initial_events
               FROM detached_spawn_intent ORDER BY created_at_ms, operation_id",
        )?;
        let intents = statement
            .query_map([], decode_detached_spawn_intent)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(anyhow::Error::from)?;
        for intent in &intents {
            validate_detached_spawn_intent_record(intent)?;
        }
        Ok(intents)
    }

    pub fn abort_unsealed_detached_spawn_intent(&self, operation_id: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM detached_spawn_intent
             WHERE operation_id=?1 AND launch_metadata IS NULL",
            params![operation_id],
        )? > 0)
    }

    /// CAS objects referenced by durable handoff intents before their child
    /// rows become authoritative chain roots. GC must retain these roots
    /// for the entire intent lifetime, including crash recovery between
    /// authority sealing and child birth.
    pub fn handoff_cas_object_roots(&self) -> Result<Vec<String>> {
        let mut roots = BTreeSet::new();
        for intent in self.detached_spawn_intents()? {
            if let Some(hash) = intent.admitted_launch_capsule_hash {
                roots.insert(hash);
            }
            if let Some(authority) = intent.child_project_authority.as_ref() {
                if let Some(hash) = authority.subject_base_snapshot_hash() {
                    roots.insert(hash.to_string());
                }
                if let Some(hash) = authority.operational_snapshot_projection() {
                    roots.insert(hash.to_string());
                }
            }
        }
        for waiter in self.list_follow_waiters()? {
            if let Some(authority) = waiter.child_project_authority.as_ref() {
                if let Some(hash) = authority.subject_base_snapshot_hash() {
                    roots.insert(hash.to_string());
                }
                if let Some(hash) = authority.operational_snapshot_projection() {
                    roots.insert(hash.to_string());
                }
            }
        }
        Ok(roots.into_iter().collect())
    }

    pub fn wait_for_project_authority(
        &self,
        thread_id: &str,
        reason: &str,
        detail: &str,
        now_ms: i64,
        deadline_at_ms: i64,
    ) -> Result<RecoveryWaitDisposition> {
        validate_runtime_thread_id(thread_id)?;
        validate_bounded_runtime_text("recovery wait reason", reason, 128)?;
        validate_bounded_runtime_text("recovery wait detail", detail, 4096)?;
        if deadline_at_ms <= now_ms {
            bail!("recovery wait deadline must be later than its start");
        }
        self.conn.execute(
            "INSERT INTO thread_recovery_wait(
                thread_id, reason, detail, started_at_ms, deadline_at_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(thread_id) DO UPDATE SET
                reason=excluded.reason,
                detail=excluded.detail",
            params![thread_id, reason, detail, now_ms, deadline_at_ms],
        )?;
        self.recovery_wait(thread_id)?
            .ok_or_else(|| anyhow!("recovery wait disappeared for thread {thread_id}"))
    }

    pub fn recovery_wait(&self, thread_id: &str) -> Result<Option<RecoveryWaitDisposition>> {
        self.conn
            .query_row(
                "SELECT thread_id, reason, detail, started_at_ms, deadline_at_ms
                   FROM thread_recovery_wait WHERE thread_id=?1",
                params![thread_id],
                |row| {
                    Ok(RecoveryWaitDisposition {
                        thread_id: row.get(0)?,
                        reason: row.get(1)?,
                        detail: row.get(2)?,
                        started_at_ms: row.get(3)?,
                        deadline_at_ms: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(anyhow::Error::from)
    }

    pub fn clear_recovery_wait(&self, thread_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM thread_recovery_wait WHERE thread_id=?1",
            params![thread_id],
        )?;
        Ok(())
    }

    /// Seal the exact callback response for a previously reserved dispatch.
    /// Repeating the same completion is harmless; any divergent completion is
    /// an integrity failure.
    pub fn complete_hook_dispatch(
        &self,
        dispatch_key: &str,
        request_hash: &str,
        response: &Value,
    ) -> Result<CompletedHookDispatch> {
        validate_sha256("dispatch_key", dispatch_key)?;
        validate_sha256("request_hash", request_hash)?;
        serde_json::from_value::<ryeos_runtime::callback_contract::CallbackDispatchResponse>(
            response.clone(),
        )
        .context("hook dispatch response violates CallbackDispatchResponse")?;
        let response_json = lillux::canonical_json(response)
            .context("canonicalize hook dispatch response")?
            .into_bytes();
        if response_json.len() > MAX_HOOK_DISPATCH_RESPONSE_BYTES {
            bail!(
                "hook dispatch response exceeds {} byte limit",
                MAX_HOOK_DISPATCH_RESPONSE_BYTES
            );
        }
        let response_hash = lillux::sha256_hex(&response_json);
        let now = lillux::time::timestamp_millis() as i64;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        type ExistingHookDispatch = (
            String,
            String,
            String,
            String,
            String,
            String,
            Option<Vec<u8>>,
            Option<String>,
        );
        let existing: Option<ExistingHookDispatch> = tx
            .query_row(
                "SELECT chain_root_id, caller_thread_id, event, hook_id,
                        request_hash, status, response_json, response_hash
                   FROM hook_dispatch_ledger WHERE dispatch_key=?1",
                params![dispatch_key],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            chain_root_id,
            occurrence_thread_id,
            event,
            hook_id,
            stored_request_hash,
            status,
            stored_json,
            stored_hash,
        )) = existing
        else {
            bail!("hook dispatch `{dispatch_key}` was not reserved");
        };
        if stored_request_hash != request_hash {
            bail!("hook dispatch `{dispatch_key}` request hash changed before completion");
        }

        match HookDispatchStatus::parse(&status)? {
            HookDispatchStatus::Pending => {
                let updated = tx.execute(
                    "UPDATE hook_dispatch_ledger
                        SET status=?3, response_json=?4, response_hash=?5, completed_at_ms=?6
                      WHERE dispatch_key=?1 AND request_hash=?2 AND status=?7",
                    params![
                        dispatch_key,
                        request_hash,
                        HookDispatchStatus::Completed.as_str(),
                        response_json,
                        response_hash,
                        now,
                        HookDispatchStatus::Pending.as_str(),
                    ],
                )?;
                if updated != 1 {
                    bail!("hook dispatch `{dispatch_key}` lost its pending reservation");
                }
            }
            HookDispatchStatus::Completed => {
                let replay = decode_completed_hook_response(
                    dispatch_key,
                    stored_json.as_deref(),
                    stored_hash.as_deref(),
                )?;
                let replay_json = lillux::canonical_json(&replay)
                    .context("canonicalize replayed hook dispatch response")?
                    .into_bytes();
                if replay_json != response_json {
                    bail!("hook dispatch `{dispatch_key}` has a divergent completion");
                }
            }
        }
        tx.commit()?;
        decode_completed_hook_dispatch(
            dispatch_key,
            chain_root_id,
            occurrence_thread_id,
            event,
            hook_id,
            stored_request_hash,
            Some(&response_json),
            Some(&response_hash),
        )
    }

    pub fn completed_hook_dispatch(
        &self,
        dispatch_key: &str,
        request_hash: &str,
    ) -> Result<CompletedHookDispatch> {
        validate_sha256("dispatch_key", dispatch_key)?;
        validate_sha256("request_hash", request_hash)?;
        type Stored = (
            String,
            String,
            String,
            String,
            String,
            String,
            Option<Vec<u8>>,
            Option<String>,
        );
        let stored: Option<Stored> = self
            .conn
            .query_row(
                "SELECT chain_root_id, caller_thread_id, event, hook_id,
                        request_hash, status, response_json, response_hash
                   FROM hook_dispatch_ledger WHERE dispatch_key=?1",
                params![dispatch_key],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            chain_root_id,
            occurrence_thread_id,
            event,
            hook_id,
            stored_request_hash,
            status,
            response_json,
            response_hash,
        )) = stored
        else {
            bail!("hook dispatch `{dispatch_key}` was not reserved");
        };
        if stored_request_hash != request_hash {
            bail!("hook dispatch `{dispatch_key}` request hash mismatch");
        }
        if HookDispatchStatus::parse(&status)? != HookDispatchStatus::Completed {
            bail!("hook dispatch `{dispatch_key}` is not completed");
        }
        decode_completed_hook_dispatch(
            dispatch_key,
            chain_root_id,
            occurrence_thread_id,
            event,
            hook_id,
            stored_request_hash,
            response_json.as_deref(),
            response_hash.as_deref(),
        )
    }

    pub fn open(path: &Path) -> Result<Self> {
        Self::open_bound(path, RuntimeDbOpenMode::Current)
    }

    /// Explicit destructive-reset counterpart for an offline caller that owns
    /// the database parent namespace but cannot share its pinned authority.
    pub(crate) fn open_for_explicit_history_reset(path: &Path) -> Result<Self> {
        Self::open_bound(path, RuntimeDbOpenMode::ExplicitHistoryReset)
    }

    /// Non-creating inspection counterpart for an offline reset dry-run. This
    /// may classify an incompatible predecessor store and expose
    /// `reset_required`, but it never mutates or replaces that store.
    pub(crate) fn open_existing_for_explicit_history_reset(path: &Path) -> Result<Self> {
        Self::open_bound(path, RuntimeDbOpenMode::ExplicitHistoryResetInspection)
    }

    /// Open the persisted runtime database for offline projection recovery
    /// without creating or migrating anything. Pending head transitions use
    /// this as fail-closed liveness authority, so an absent or stale database
    /// must never be replaced by a fresh empty one.
    pub fn open_existing_current(path: &Path) -> Result<Self> {
        Self::open_bound(path, RuntimeDbOpenMode::ExistingCurrent)
    }

    /// Open the live runtime store beneath the daemon's already pinned and
    /// exclusively locked runtime-state namespace.
    pub(crate) fn open_with_namespace_authority(
        path: &Path,
        directory: lillux::PinnedDirectory,
        directory_lock: lillux::PinnedDirectoryLock,
    ) -> Result<Self> {
        Self::open_bound_in_directory(path, RuntimeDbOpenMode::Current, directory, directory_lock)
    }

    /// Open beneath an already pinned offline namespace, destructively
    /// resetting an incompatible owned runtime schema only when the operator
    /// has explicitly confirmed retirement of all thread history.
    pub(crate) fn open_for_explicit_history_reset_with_namespace_authority(
        path: &Path,
        directory: lillux::PinnedDirectory,
        directory_lock: lillux::PinnedDirectoryLock,
    ) -> Result<Self> {
        Self::open_bound_in_directory(
            path,
            RuntimeDbOpenMode::ExplicitHistoryReset,
            directory,
            directory_lock,
        )
    }

    /// Non-creating inspection counterpart for an offline reset dry-run under
    /// the caller's already pinned runtime-state namespace.
    pub(crate) fn open_existing_for_explicit_history_reset_with_namespace_authority(
        path: &Path,
        directory: lillux::PinnedDirectory,
        directory_lock: lillux::PinnedDirectoryLock,
    ) -> Result<Self> {
        Self::open_bound_in_directory(
            path,
            RuntimeDbOpenMode::ExplicitHistoryResetInspection,
            directory,
            directory_lock,
        )
    }

    fn open_bound(path: &Path, mode: RuntimeDbOpenMode) -> Result<Self> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let directory = if mode.allow_create() {
            lillux::PinnedDirectory::open_or_create(parent)
                .with_context(|| format!("pin runtime database parent {}", parent.display()))?
        } else {
            lillux::PinnedDirectory::open(parent)
                .with_context(|| format!("pin runtime database parent {}", parent.display()))?
                .ok_or_else(|| {
                    anyhow::anyhow!("runtime database parent is absent: {}", parent.display())
                })?
        };
        ensure_runtime_directory_binding(&directory)?;
        let directory_lock = directory
            .lock_exclusive()
            .context("lock runtime database parent")?;
        Self::open_bound_in_directory(path, mode, directory, directory_lock)
    }

    fn open_bound_in_directory(
        path: &Path,
        mode: RuntimeDbOpenMode,
        directory: lillux::PinnedDirectory,
        directory_lock: lillux::PinnedDirectoryLock,
    ) -> Result<Self> {
        let name = path
            .file_name()
            .ok_or_else(|| {
                anyhow::anyhow!("runtime database path has no filename: {}", path.display())
            })?
            .to_os_string();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        if directory.path() != parent {
            bail!(
                "runtime database namespace authority path mismatch: selected={}, requested={}",
                directory.path().display(),
                parent.display()
            );
        }
        ensure_runtime_directory_binding(&directory)?;
        directory_lock
            .ensure_protects(&directory)
            .context("verify runtime database namespace lock")?;
        inspect_runtime_sidecars(&directory, &name)?;

        if mode.materializes_inspection_copy() {
            let (inspection_directory, inspection_guard) =
                create_sqlite_inspection_copy(&directory, &name, "runtime")?;
            let inspection_path = inspection_directory.path().join(&name);
            let inspection_lock = inspection_directory
                .lock_exclusive()
                .context("lock runtime inspection copy")?;
            let mut opened = Self::open_bound_in_directory(
                &inspection_path,
                RuntimeDbOpenMode::ExplicitHistoryResetInspectionCopy,
                inspection_directory,
                inspection_lock,
            )?;
            opened.open_mode = RuntimeDbOpenMode::ExplicitHistoryResetInspection;
            opened._inspection_copy = Some(inspection_guard);
            return Ok(opened);
        }

        let existing = directory.open_regular(&name, true).with_context(|| {
            format!(
                "runtime database must be a regular non-symlink file: {}",
                path.display()
            )
        })?;
        let (database_file, created) = match existing {
            Some(file) => (file, false),
            None if mode.allow_create() => {
                let file = directory
                    .open_regular_create(&name, true, true, 0o600)
                    .with_context(|| format!("create runtime database {}", path.display()))?;
                directory.sync().context("sync runtime database creation")?;
                (file, true)
            }
            None => bail!("runtime database is absent: {}", path.display()),
        };
        let descriptors_before = matching_open_descriptors(&database_file)?;
        let wal_name = runtime_sidecar_name(&name, "-wal");
        let shm_name = runtime_sidecar_name(&name, "-shm");
        let wal_before = directory.open_regular(&wal_name, false)?;
        let shm_before = directory.open_regular(&shm_name, false)?;
        let wal_descriptors_before = wal_before
            .as_ref()
            .map(matching_open_descriptors)
            .transpose()?
            .unwrap_or_default();
        let shm_descriptors_before = shm_before
            .as_ref()
            .map(matching_open_descriptors)
            .transpose()?
            .unwrap_or_default();
        ensure_runtime_file_binding(&directory, &name, &database_file, "runtime database")?;

        let descriptor_path = directory.descriptor_child_path(&name)?;
        let conn = Connection::open_with_flags(&descriptor_path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .with_context(|| format!("open runtime database {}", path.display()))?;
        ensure_runtime_directory_binding(&directory)?;
        ensure_runtime_file_binding(&directory, &name, &database_file, "runtime database")?;
        ensure_sqlite_connection_uses_expected_file(
            &database_file,
            &descriptors_before,
            "runtime database",
        )?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .context("enable runtime database foreign keys")?;
        if created {
            conn.execute_batch("PRAGMA journal_mode=WAL;")
                .context("establish WAL for the current runtime store")?;
        }

        let mut reset_required = false;
        if created {
            initialize_current_runtime_schema(&conn, path)?;
        } else if let Err(error) = validate_current_runtime_store(&conn, path) {
            if !mode.explicit_history_reset() {
                if is_newer_execution_schema(&error) {
                    return Err(error);
                }
                if requires_execution_schema_cutover(&error) {
                    return Err(error).context(format!(
                        "runtime database contains predecessor execution authority and requires the explicit no-backcompat reset ({})",
                        path.display()
                    ));
                }
                return Err(error);
            }
            if is_newer_execution_schema(&error) {
                return Err(error).context(
                    "runtime database contains execution authority newer than this RyeOS binary; refusing destructive reset",
                );
            }
            if !requires_execution_schema_cutover(&error) {
                return Err(error).context(
                    "runtime database validation failed without a proven predecessor schema epoch; refusing destructive reset",
                );
            }
            // Do not mutate yet. Offline GC first publishes the authoritative
            // cross-store discard intent; only then may it call
            // `apply_explicit_history_reset` on this pinned handle.
            reset_required = true;
        }
        if reset_required {
            let integrity: String = conn
                .query_row("PRAGMA integrity_check", [], |row| row.get(0))
                .context("verify incompatible runtime database integrity before reset")?;
            if integrity != "ok" {
                bail!(
                    "runtime database integrity check failed for {}: {integrity}",
                    path.display()
                );
            }
            return Ok(Self {
                conn,
                reset_required: true,
                open_mode: mode,
                _directory: Some(directory),
                _directory_lock: Some(directory_lock),
                _database_file: Some(database_file),
                _wal_file: wal_before,
                _shm_file: shm_before,
                _inspection_copy: None,
            });
        }
        validate_current_runtime_store(&conn, path)?;
        let integrity_started = std::time::Instant::now();
        let database_bytes = database_file.metadata()?.len();
        tracing::info!(
            database_bytes,
            "verifying retained runtime database integrity"
        );
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .context("verify runtime database integrity")?;
        if integrity != "ok" {
            bail!(
                "runtime database integrity check failed for {}: {integrity}",
                path.display()
            );
        }
        tracing::info!(
            database_bytes,
            duration_ms = integrity_started.elapsed().as_millis(),
            "retained runtime database integrity verified"
        );
        let journal_mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .context("read runtime database journal mode")?;
        if journal_mode != "wal" {
            bail!(
                "runtime database journal mode mismatch in {}: stored={journal_mode}, expected=wal",
                path.display()
            );
        }
        conn.execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
            .context("eagerly establish runtime database WAL handles")?;
        let wal_file = directory.open_regular(&wal_name, false)?.ok_or_else(|| {
            anyhow::anyhow!(
                "SQLite did not establish runtime WAL: {}",
                directory.path().join(&wal_name).display()
            )
        })?;
        let shm_file = directory.open_regular(&shm_name, false)?.ok_or_else(|| {
            anyhow::anyhow!(
                "SQLite did not establish runtime shared memory: {}",
                directory.path().join(&shm_name).display()
            )
        })?;
        if let Some(expected) = wal_before.as_ref() {
            ensure_same_runtime_file(expected, &wal_file, "runtime WAL", path)?;
        }
        if let Some(expected) = shm_before.as_ref() {
            ensure_same_runtime_file(expected, &shm_file, "runtime shared memory", path)?;
        }
        ensure_sqlite_connection_uses_expected_file(
            &wal_file,
            &wal_descriptors_before,
            "runtime WAL",
        )?;
        ensure_sqlite_connection_uses_expected_file(
            &shm_file,
            &shm_descriptors_before,
            "runtime shared memory",
        )?;
        ensure_runtime_directory_binding(&directory)?;
        ensure_runtime_file_binding(&directory, &name, &database_file, "runtime database")?;
        ensure_runtime_file_binding(&directory, &wal_name, &wal_file, "runtime WAL")?;
        ensure_runtime_file_binding(&directory, &shm_name, &shm_file, "runtime shared memory")?;

        Ok(Self {
            conn,
            reset_required: false,
            open_mode: mode,
            _directory: Some(directory),
            _directory_lock: Some(directory_lock),
            _database_file: Some(database_file),
            _wal_file: Some(wal_file),
            _shm_file: Some(shm_file),
            _inspection_copy: None,
        })
    }

    pub fn requires_explicit_history_reset(&self) -> bool {
        self.reset_required
    }

    /// Apply the already-confirmed destructive schema cutover. Callers must
    /// publish their authoritative cross-store discard intent before invoking
    /// this method; opening the pinned handle never performs this mutation.
    pub fn apply_explicit_history_reset(&mut self, path: &Path) -> Result<()> {
        if self.open_mode == RuntimeDbOpenMode::ExplicitHistoryResetInspection {
            bail!("runtime inspection authority cannot apply an explicit history reset");
        }
        if !self.reset_required {
            return Ok(());
        }
        self.conn
            .execute_batch("PRAGMA journal_mode=WAL;")
            .context("establish WAL for explicitly reset runtime store")?;
        reset_owned_runtime_schema(&self.conn, path)?;
        validate_current_runtime_store(&self.conn, path)?;
        self.reset_required = false;
        Ok(())
    }

    pub fn insert_thread_runtime(&self, thread_id: &str, chain_root_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO thread_runtime (thread_id, chain_root_id, pid, pgid, metadata, launch_metadata)
             VALUES (?1, ?2, NULL, NULL, NULL, NULL)",
            params![thread_id, chain_root_id],
        )?;
        Ok(())
    }

    /// Atomically create the auxiliary runtime row and the current-schema
    /// durable reservation which makes an in-process birth reconcilable across
    /// a crash before its authoritative CAS head is published.
    pub fn reserve_in_process_handler_birth(
        &self,
        thread_id: &str,
        chain_root_id: &str,
        launch_metadata: &RuntimeLaunchMetadata,
    ) -> Result<()> {
        launch_metadata.validate()?;
        if launch_metadata.launch_driver
            != Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
        {
            bail!("in-process birth reservation requires the in-process launch driver");
        }
        let launch_metadata = encode_current_launch_metadata(launch_metadata)
            .context("encode in-process birth launch metadata")?;
        let now = lillux::time::timestamp_millis() as i64;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let reservations: i64 = tx.query_row(
            "SELECT COUNT(*) FROM in_process_handler_reservation",
            [],
            |row| row.get(0),
        )?;
        if usize::try_from(reservations)
            .context("in-process handler reservation count is invalid")?
            >= MAX_IN_PROCESS_HANDLER_RESERVATIONS
        {
            bail!(
                "in-process handler reservations reached the current-schema limit of {MAX_IN_PROCESS_HANDLER_RESERVATIONS}"
            );
        }
        tx.execute(
            "INSERT INTO thread_runtime (
                thread_id, chain_root_id, pid, pgid, metadata, launch_metadata
             ) VALUES (?1, ?2, NULL, NULL, NULL, ?3)",
            params![thread_id, chain_root_id, launch_metadata],
        )?;
        tx.execute(
            "INSERT INTO in_process_handler_reservation (
                thread_id, phase, created_at_ms, updated_at_ms
             ) VALUES (?1, 'pending', ?2, ?2)",
            params![thread_id, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Advance the exact pending reservation after the CAS root has committed.
    /// Repeating the transition is idempotent; a missing or contradictory row
    /// is an integrity failure.
    pub fn mark_in_process_handler_birth_running(&self, thread_id: &str) -> Result<()> {
        let now = lillux::time::timestamp_millis() as i64;
        let updated = self.conn.execute(
            "UPDATE in_process_handler_reservation
                SET phase = 'running', updated_at_ms = ?2
              WHERE thread_id = ?1 AND phase = 'pending'",
            params![thread_id, now],
        )?;
        if updated == 1 {
            return Ok(());
        }
        let phase = self
            .conn
            .query_row(
                "SELECT phase FROM in_process_handler_reservation WHERE thread_id = ?1",
                params![thread_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        match phase.as_deref() {
            Some("running") => Ok(()),
            Some(other) => {
                bail!("in-process handler reservation `{thread_id}` has invalid phase `{other}`")
            }
            None => bail!("in-process handler reservation `{thread_id}` is missing"),
        }
    }

    /// Remove a birth which never acquired an authoritative CAS root. Both
    /// rows are deleted in one SQLite transaction, and only a pending
    /// reservation may authorize this cleanup.
    pub fn discard_pending_in_process_handler_birth(&self, thread_id: &str) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let phase = tx
            .query_row(
                "SELECT phase FROM in_process_handler_reservation WHERE thread_id = ?1",
                params![thread_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(phase) = phase else {
            tx.commit()?;
            return Ok(false);
        };
        if phase != "pending" {
            bail!("refusing to discard in-process handler birth `{thread_id}` in phase `{phase}`");
        }
        let runtime_deleted = tx.execute(
            "DELETE FROM thread_runtime WHERE thread_id = ?1",
            params![thread_id],
        )?;
        if runtime_deleted != 1 {
            bail!("pending in-process handler birth `{thread_id}` has no exact runtime row");
        }
        let reservation_deleted = tx.execute(
            "DELETE FROM in_process_handler_reservation WHERE thread_id = ?1 AND phase = 'pending'",
            params![thread_id],
        )?;
        if reservation_deleted != 1 {
            bail!("pending in-process handler reservation `{thread_id}` disappeared");
        }
        tx.commit()?;
        Ok(true)
    }

    /// Persist exact terminal confirmation as an idempotent reservation phase.
    /// Cleanup is separate so an ambiguous acknowledgement can be retried
    /// without interpreting absence as success.
    pub fn settle_in_process_handler_reservation(&self, thread_id: &str) -> Result<bool> {
        let now = lillux::time::timestamp_millis() as i64;
        let updated = self.conn.execute(
            "UPDATE in_process_handler_reservation
                SET phase = 'terminal_confirmed', updated_at_ms = ?2
              WHERE thread_id = ?1 AND phase IN ('pending', 'running')",
            params![thread_id, now],
        )?;
        if updated == 1 {
            return Ok(true);
        }
        let phase = self
            .conn
            .query_row(
                "SELECT phase FROM in_process_handler_reservation WHERE thread_id = ?1",
                params![thread_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        match phase.as_deref() {
            Some("terminal_confirmed") => Ok(true),
            Some(other) => bail!(
                "in-process handler reservation `{thread_id}` has invalid terminal phase `{other}`"
            ),
            None => Ok(false),
        }
    }

    pub fn delete_terminal_in_process_handler_reservation(&self, thread_id: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM in_process_handler_reservation
              WHERE thread_id = ?1 AND phase = 'terminal_confirmed'",
            params![thread_id],
        )? > 0)
    }

    pub fn in_process_handler_reservations_after(
        &self,
        after_thread_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<InProcessHandlerReservation>> {
        if limit == 0 || limit > IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE {
            bail!(
                "in-process handler reservation limit must be 1..={IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE}"
            );
        }
        let mut stmt = self.conn.prepare(
            "SELECT thread_id, phase
               FROM in_process_handler_reservation
              WHERE (?1 IS NULL OR thread_id > ?1)
              ORDER BY thread_id
              LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![after_thread_id, limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (thread_id, phase) = row?;
            Ok(InProcessHandlerReservation {
                thread_id,
                phase: InProcessHandlerReservationPhase::parse(&phase)?,
            })
        })
        .collect()
    }

    pub fn in_process_handler_reservation(
        &self,
        thread_id: &str,
    ) -> Result<Option<InProcessHandlerReservation>> {
        let row = self
            .conn
            .query_row(
                "SELECT phase FROM in_process_handler_reservation WHERE thread_id = ?1",
                params![thread_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        row.map(|phase| {
            Ok(InProcessHandlerReservation {
                thread_id: thread_id.to_string(),
                phase: InProcessHandlerReservationPhase::parse(&phase)?,
            })
        })
        .transpose()
    }

    /// Atomically seed the runtime identity for a continuation successor.
    ///
    /// A daemon crash may leave this row behind before the authoritative state
    /// handoff commits. Re-seeding that exact unattached orphan is idempotent;
    /// any attached, stopped, claimed, or cross-chain row fails closed.
    pub fn seed_continuation_runtime(
        &self,
        thread_id: &str,
        chain_root_id: &str,
        launch_metadata: &RuntimeLaunchMetadata,
    ) -> Result<()> {
        if launch_metadata.launch_driver
            == Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
        {
            bail!("in-process handlers cannot use continuation runtime seeding");
        }
        let launch_metadata_json = encode_current_launch_metadata(launch_metadata)
            .context("failed to encode continuation launch_metadata")?;
        let tx = self.conn.unchecked_transaction()?;
        let existing = tx
            .query_row(
                "SELECT chain_root_id, pid, pgid, process_identity,
                        stop_requested_at_ms, stop_intent, metadata, launch_metadata
                   FROM thread_runtime WHERE thread_id = ?1",
                params![thread_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<Vec<u8>>>(6)?,
                        row.get::<_, Option<String>>(7)?,
                    ))
                },
            )
            .optional()?;
        let claimed: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM thread_launch_claim WHERE thread_id = ?1)",
            params![thread_id],
            |row| row.get(0),
        )?;
        if claimed {
            bail!("continuation runtime row {thread_id} already has a launch claim");
        }

        if let Some((
            existing_chain_root_id,
            pid,
            pgid,
            process_identity,
            stop_requested_at_ms,
            stop_intent,
            metadata,
            existing_launch_metadata,
        )) = existing
        {
            if existing_chain_root_id != chain_root_id {
                bail!(
                    "continuation runtime row {thread_id} belongs to chain {existing_chain_root_id}, not {chain_root_id}"
                );
            }
            if pid.is_some()
                || pgid.is_some()
                || process_identity.is_some()
                || stop_requested_at_ms.is_some()
                || stop_intent.is_some()
                || metadata.as_deref() != Some(CONTINUATION_SEED_MARKER)
                || existing_launch_metadata.as_deref() != Some(launch_metadata_json.as_str())
            {
                bail!(
                    "continuation runtime row {thread_id} is not the exact unattached, unclaimed seed"
                );
            }
        } else {
            tx.execute(
                "INSERT INTO thread_runtime
                    (thread_id, chain_root_id, pid, pgid, metadata, launch_metadata)
                 VALUES (?1, ?2, NULL, NULL, ?3, ?4)",
                params![
                    thread_id,
                    chain_root_id,
                    CONTINUATION_SEED_MARKER,
                    launch_metadata_json
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Remove a continuation runtime seed after its authoritative state commit
    /// failed. The exact metadata match prevents cleanup from deleting a row
    /// that another owner has repurposed since it was seeded.
    pub fn remove_seeded_continuation_runtime(
        &self,
        thread_id: &str,
        chain_root_id: &str,
        launch_metadata: &RuntimeLaunchMetadata,
    ) -> Result<bool> {
        let launch_metadata_json = encode_current_launch_metadata(launch_metadata)
            .context("failed to encode continuation launch_metadata for cleanup")?;
        Ok(self.conn.execute(
            "DELETE FROM thread_runtime
              WHERE thread_id = ?1
                AND chain_root_id = ?2
                AND launch_metadata = ?3
                AND metadata = ?4
                AND pid IS NULL
                AND pgid IS NULL
                AND process_identity IS NULL
                AND stop_requested_at_ms IS NULL
                AND stop_intent IS NULL
                AND NOT EXISTS (
                    SELECT 1 FROM thread_launch_claim
                     WHERE thread_launch_claim.thread_id = thread_runtime.thread_id
                )",
            params![
                thread_id,
                chain_root_id,
                launch_metadata_json,
                CONTINUATION_SEED_MARKER
            ],
        )? > 0)
    }

    pub fn continuation_seed_rows_after(
        &self,
        after_thread_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        if limit == 0 || limit > CONTINUATION_SEED_RECONCILE_PAGE_SIZE {
            bail!(
                "continuation seed reconcile limit must be 1..={CONTINUATION_SEED_RECONCILE_PAGE_SIZE}"
            );
        }
        let mut stmt = self.conn.prepare(
            "SELECT thread_id, chain_root_id
               FROM thread_runtime
              WHERE metadata = ?1
                AND (?2 IS NULL OR thread_id > ?2)
              ORDER BY thread_id
              LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(
                params![CONTINUATION_SEED_MARKER, after_thread_id, limit as i64],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn clear_continuation_seed_marker(
        &self,
        thread_id: &str,
        chain_root_id: &str,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE thread_runtime
                SET metadata = NULL
              WHERE thread_id = ?1
                AND chain_root_id = ?2
                AND metadata = ?3",
            params![thread_id, chain_root_id, CONTINUATION_SEED_MARKER],
        )? > 0)
    }

    pub fn remove_orphaned_continuation_seed(
        &self,
        thread_id: &str,
        chain_root_id: &str,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM thread_runtime
              WHERE thread_id = ?1
                AND chain_root_id = ?2
                AND metadata = ?3
                AND pid IS NULL
                AND pgid IS NULL
                AND process_identity IS NULL
                AND stop_requested_at_ms IS NULL
                AND stop_intent IS NULL
                AND NOT EXISTS (
                    SELECT 1 FROM thread_launch_claim
                     WHERE thread_launch_claim.thread_id = thread_runtime.thread_id
                )",
            params![thread_id, chain_root_id, CONTINUATION_SEED_MARKER],
        )? > 0)
    }

    pub fn delete_thread_runtime(&self, thread_id: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM thread_runtime
              WHERE thread_id = ?1
                AND NOT EXISTS (
                    SELECT 1 FROM in_process_handler_reservation
                     WHERE in_process_handler_reservation.thread_id = thread_runtime.thread_id
                )",
            params![thread_id],
        )?)
    }

    pub fn touch_seat_lease(
        &self,
        seat_thread_id: &str,
        owner: &str,
        surface: &str,
        client_ref: &str,
    ) -> Result<bool> {
        let now = lillux::time::timestamp_millis() as i64;
        Ok(self.conn.execute(
            "INSERT INTO seat_lease
                (seat_thread_id, owner, surface, client_ref, last_seen_at_ms, reaping_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL)
             ON CONFLICT(seat_thread_id) DO UPDATE SET
                owner=excluded.owner, surface=excluded.surface,
                client_ref=excluded.client_ref, last_seen_at_ms=excluded.last_seen_at_ms
             WHERE seat_lease.reaping_at_ms IS NULL",
            params![seat_thread_id, owner, surface, client_ref, now],
        )? > 0)
    }

    pub fn touch_existing_seat_lease(&self, seat_thread_id: &str) -> Result<bool> {
        let now = lillux::time::timestamp_millis() as i64;
        Ok(self.conn.execute(
            "UPDATE seat_lease SET last_seen_at_ms=?2
             WHERE seat_thread_id=?1 AND reaping_at_ms IS NULL",
            params![seat_thread_id, now],
        )? > 0)
    }

    pub fn remove_seat_lease(&self, seat_thread_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM seat_lease WHERE seat_thread_id=?1",
            params![seat_thread_id],
        )?;
        Ok(())
    }

    pub fn expired_seat_leases(&self, cutoff_ms: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT seat_thread_id FROM seat_lease WHERE last_seen_at_ms < ?1 ORDER BY last_seen_at_ms",
        )?;
        let rows = stmt.query_map(params![cutoff_ms], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn claim_expired_seat_lease(&self, seat_thread_id: &str, cutoff_ms: i64) -> Result<bool> {
        let now = lillux::time::timestamp_millis() as i64;
        Ok(self.conn.execute(
            "UPDATE seat_lease SET reaping_at_ms=?3
             WHERE seat_thread_id=?1 AND last_seen_at_ms < ?2",
            params![seat_thread_id, cutoff_ms, now],
        )? > 0)
    }

    pub fn inspect_chain_recovery_pins(
        &self,
        chain_root_id: &str,
        thread_ids: &[String],
    ) -> Result<ChainRecoveryPins> {
        let count = |sql: &str| -> Result<u64> {
            let value: i64 = self
                .conn
                .query_row(sql, params![chain_root_id], |row| row.get(0))?;
            u64::try_from(value).context("negative recovery-pin count")
        };
        let count_thread = |sql: &str, thread_id: &str| -> Result<u64> {
            let value: i64 = self
                .conn
                .query_row(sql, params![thread_id], |row| row.get(0))?;
            u64::try_from(value).context("negative thread recovery-pin count")
        };
        let parent_follow_waiters =
            count("SELECT COUNT(*) FROM follow_waiter WHERE parent_chain_root_id=?1")?;
        let follow_waiters = count(
            "SELECT
                (SELECT COUNT(*) FROM follow_waiter
                 WHERE parent_chain_root_id=?1)
              + (SELECT COUNT(*) FROM follow_waiter_child
                 WHERE child_chain_root_id=?1)",
        )?;
        let launch_windows =
            count("SELECT COUNT(*) FROM launch_window WHERE child_chain_root_id=?1")?;
        let cancelled_launch_windows = count(
            "SELECT COUNT(*) FROM launch_window
             WHERE child_chain_root_id=?1 AND cancelled_at_ms IS NOT NULL",
        )?;
        let mut pins = ChainRecoveryPins {
            // A parent follow waiter owns the graph checkpoint until its
            // successor is durably resumed or the waiter is otherwise settled.
            required_checkpoint_consumers: parent_follow_waiters,
            cancellation_repairs: cancelled_launch_windows,
            follow_waiters,
            launch_windows,
            ..Default::default()
        };
        let authoritative_members = thread_ids
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let mut runtime_members = self
            .conn
            .prepare("SELECT thread_id FROM thread_runtime WHERE chain_root_id=?1")?
            .query_map(params![chain_root_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        runtime_members.sort();
        for runtime_thread_id in runtime_members {
            if !authoritative_members.contains(runtime_thread_id.as_str()) {
                add_pin_count(
                    &mut pins.runtime_membership_conflicts,
                    1,
                    "runtime-membership-conflict",
                )?;
            }
        }
        for thread_id in thread_ids {
            let runtime_chain_root_id = self
                .conn
                .query_row(
                    "SELECT chain_root_id FROM thread_runtime WHERE thread_id=?1",
                    params![thread_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if runtime_chain_root_id
                .as_deref()
                .is_some_and(|runtime_chain_root_id| runtime_chain_root_id != chain_root_id)
            {
                add_pin_count(
                    &mut pins.runtime_membership_conflicts,
                    1,
                    "runtime-membership-conflict",
                )?;
            }
            // Decode launch metadata loudly. Corrupt recovery ownership is an
            // unreadable pin set and therefore fails retention closed.
            let runtime_info = self.get_runtime_info(thread_id)?;
            let live = match runtime_info.as_ref() {
                Some(RuntimeInfo {
                    pgid: Some(pgid), ..
                }) => crate::process::pgid_live_for_retention(*pgid)?,
                Some(RuntimeInfo { pid: Some(pid), .. }) => {
                    crate::process::pid_live_for_retention(*pid)?
                }
                Some(RuntimeInfo {
                    pid: None,
                    pgid: None,
                    ..
                })
                | None => false,
            };
            if live {
                add_pin_count(&mut pins.live_processes, 1, "live-process")?;
            }
            let launch_claims = count_thread(
                "SELECT COUNT(*) FROM thread_launch_claim WHERE thread_id=?1",
                thread_id,
            )?;
            let in_process_handler_reservations = count_thread(
                "SELECT COUNT(*) FROM in_process_handler_reservation WHERE thread_id=?1",
                thread_id,
            )?;
            let pending_commands = count_thread(
                "SELECT COUNT(*) FROM thread_commands
                 WHERE thread_id=?1 AND status IN ('pending','claimed')",
                thread_id,
            )?;
            let open_control_commands = count_thread(
                "SELECT COUNT(*) FROM thread_commands
                 WHERE thread_id=?1 AND status IN ('pending','claimed')
                   AND command_type IN ('cancel','kill')",
                thread_id,
            )?;
            let owners = classify_thread_recovery_owners(
                runtime_info.as_ref(),
                launch_claims,
                open_control_commands,
            );
            add_pin_count(&mut pins.launch_claims, launch_claims, "launch-claim")?;
            add_pin_count(
                &mut pins.in_process_handler_reservations,
                in_process_handler_reservations,
                "in-process-handler-reservation",
            )?;
            add_pin_count(
                &mut pins.recovery_capable_launch_claims,
                owners.recovery_capable_launch_claims,
                "recovery-capable-launch-claim",
            )?;
            add_pin_count(
                &mut pins.required_checkpoint_consumers,
                owners.required_checkpoint_consumers,
                "required-checkpoint-consumer",
            )?;
            add_pin_count(&mut pins.pending_commands, pending_commands, "open-command")?;
            add_pin_count(
                &mut pins.cancellation_repairs,
                owners.cancellation_repairs,
                "cancellation-repair",
            )?;
            let seat_leases = count_thread(
                "SELECT COUNT(*) FROM seat_lease WHERE seat_thread_id=?1",
                thread_id,
            )?;
            add_pin_count(&mut pins.seat_leases, seat_leases, "seat-lease")?;
        }
        Ok(pins)
    }

    pub fn chain_has_live_state(&self, chain_root_id: &str) -> Result<bool> {
        let mut statement = self
            .conn
            .prepare("SELECT thread_id FROM thread_runtime WHERE chain_root_id=?1")?;
        let thread_ids = statement
            .query_map(params![chain_root_id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(!self
            .inspect_chain_recovery_pins(chain_root_id, &thread_ids)?
            .is_empty())
    }

    /// Return every operational parent/child edge touching one of the supplied
    /// authoritative chain members. The StateStore combines these structural
    /// edges with projected counterpart status; the runtime DB cannot decide
    /// by itself whether an edge still pins recovery.
    pub fn chain_child_links(&self, thread_ids: &[String]) -> Result<Vec<(String, String)>> {
        let mut links = BTreeSet::new();
        let mut statement = self.conn.prepare(
            "SELECT parent_thread_id, child_thread_id FROM thread_child_link
             WHERE parent_thread_id=?1 OR child_thread_id=?1",
        )?;
        for thread_id in thread_ids {
            for row in statement.query_map(params![thread_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })? {
                links.insert(row?);
            }
        }
        Ok(links.into_iter().collect())
    }

    /// Delete operational state only after StateStore has proven that the
    /// authoritative chain is terminal, unpinned, and can no longer resume.
    /// Hook-dispatch rows must outlive every possible refire; making this
    /// crate-private prevents offline/runtime callers from treating the ledger
    /// as a disposable response cache.
    pub(crate) fn delete_chain_runtime(
        &self,
        chain_root_id: &str,
        thread_ids: &[String],
    ) -> Result<usize> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            let mut deleted = 0usize;
            deleted += self.conn.execute(
                "DELETE FROM hook_dispatch_ledger WHERE chain_root_id=?1",
                params![chain_root_id],
            )?;
            // Signed chain truth supplies the authoritative members. Include
            // any runtime row structurally attributed to the same chain so a
            // replay after the head-removal boundary cannot leave orphaned
            // operational rows. The pre-removal pin pass rejects this
            // disagreement; this union is the idempotent crash-cleanup side.
            let mut cleanup_thread_ids = thread_ids.iter().cloned().collect::<BTreeSet<_>>();
            {
                let mut statement = self
                    .conn
                    .prepare("SELECT thread_id FROM thread_runtime WHERE chain_root_id=?1")?;
                let runtime_thread_ids = statement
                    .query_map(params![chain_root_id], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                cleanup_thread_ids.extend(runtime_thread_ids);
            }
            for thread_id in cleanup_thread_ids {
                // A bound accepted-launch coordinate has exactly the lifetime
                // of its authoritative thread history. It is not a response
                // cache and must not expire while that root can still exist.
                deleted += self.conn.execute(
                    "DELETE FROM launch_planning WHERE reserved_thread_id=?1",
                    params![&thread_id],
                )?;
                deleted += self.conn.execute(
                    "DELETE FROM detached_spawn_intent WHERE parent_thread_id=?1",
                    params![&thread_id],
                )?;
                deleted += self.conn.execute(
                    "DELETE FROM thread_recovery_wait WHERE thread_id=?1",
                    params![&thread_id],
                )?;
                deleted += self.conn.execute(
                    "DELETE FROM thread_commands WHERE thread_id=?1",
                    params![&thread_id],
                )?;
                deleted += self.conn.execute(
                    "DELETE FROM thread_launch_claim WHERE thread_id=?1",
                    params![&thread_id],
                )?;
                deleted += self.conn.execute(
                    "DELETE FROM in_process_handler_reservation WHERE thread_id=?1",
                    params![&thread_id],
                )?;
                deleted += self.conn.execute(
                    "DELETE FROM seat_lease WHERE seat_thread_id=?1",
                    params![&thread_id],
                )?;
                deleted += self.conn.execute(
                    "DELETE FROM thread_child_link
                     WHERE child_thread_id=?1 OR parent_thread_id=?1",
                    params![&thread_id],
                )?;
                deleted += self.conn.execute(
                    "DELETE FROM thread_runtime WHERE thread_id=?1",
                    params![&thread_id],
                )?;
            }
            deleted += self.conn.execute(
                "DELETE FROM launch_window WHERE child_chain_root_id=?1",
                params![chain_root_id],
            )?;
            deleted += self.conn.execute(
                "DELETE FROM follow_waiter_child WHERE child_chain_root_id=?1",
                params![chain_root_id],
            )?;
            deleted += self.conn.execute(
                "DELETE FROM follow_waiter WHERE parent_chain_root_id=?1",
                params![chain_root_id],
            )?;
            Ok::<_, rusqlite::Error>(deleted)
        })();
        match result {
            Ok(deleted) => match self.conn.execute_batch("COMMIT") {
                Ok(()) => Ok(deleted),
                Err(commit_error) => {
                    let rollback_error = self.conn.execute_batch("ROLLBACK").err();
                    match rollback_error {
                        Some(rollback_error) => Err(anyhow::anyhow!(
                            "commit chain runtime cleanup failed: {commit_error}; rollback after commit failure also failed: {rollback_error}"
                        )),
                        None => Err(commit_error.into()),
                    }
                }
            },
            Err(err) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(err.into())
            }
        }
    }

    #[tracing::instrument(
        name = "state:thread_attach",
        skip(self, launch_metadata),
        fields(thread_id = %thread_id, pid = pid, pgid = pgid)
    )]
    pub fn attach_process(
        &self,
        thread_id: &str,
        pid: i64,
        pgid: i64,
        process_identity: &ExecutionProcessIdentity,
        launch_metadata: &RuntimeLaunchMetadata,
    ) -> Result<()> {
        self.attach_process_with_mode(
            thread_id,
            pid,
            pgid,
            process_identity,
            launch_metadata,
            false,
        )
    }

    /// Attach an identity at an attachment-before-execution boundary.
    /// Unlike runtime self-attachment, an exact repeat is an invariant
    /// violation here: target code has not been released and therefore could
    /// not have authored a legitimate prior attachment.
    pub fn attach_new_process(
        &self,
        thread_id: &str,
        pid: i64,
        pgid: i64,
        process_identity: &ExecutionProcessIdentity,
        launch_metadata: &RuntimeLaunchMetadata,
    ) -> Result<()> {
        self.attach_process_with_mode(
            thread_id,
            pid,
            pgid,
            process_identity,
            launch_metadata,
            true,
        )
    }

    fn attach_process_with_mode(
        &self,
        thread_id: &str,
        pid: i64,
        pgid: i64,
        process_identity: &ExecutionProcessIdentity,
        launch_metadata: &RuntimeLaunchMetadata,
        require_empty: bool,
    ) -> Result<()> {
        if launch_metadata.launch_driver
            == Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
        {
            bail!("in-process handlers cannot attach an external process identity");
        }
        if process_identity.schema_version != PROCESS_IDENTITY_SCHEMA_VERSION
            || process_identity.target_pid != pid
            || process_identity.group_leader_pid != pgid
        {
            bail!("process identity does not match attached pid/pgid for thread {thread_id}");
        }
        validate_execution_process_identity_shape(process_identity)
            .context("invalid process identity shape during attach")?;
        let identity_json =
            serde_json::to_string(process_identity).context("failed to encode process_identity")?;
        let existing = self
            .conn
            .query_row(
                "SELECT pid, pgid, process_identity, stop_requested_at_ms, launch_metadata
                   FROM thread_runtime WHERE thread_id = ?1",
                params![thread_id],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| {
                anyhow::anyhow!("thread_runtime row missing for thread_id: {thread_id}")
            })?;
        let (
            existing_pid,
            existing_pgid,
            existing_identity,
            stop_requested_at_ms,
            existing_launch_metadata,
        ) = existing;
        let existing_launch_metadata = existing_launch_metadata
            .map(|raw| decode_current_launch_metadata(&raw))
            .transpose()?;
        let merged_launch_metadata = match existing_launch_metadata {
            Some(authoritative) if launch_metadata.is_empty() => Some(authoritative),
            Some(authoritative) => Some(authoritative.merge_for_process_attach(launch_metadata)?),
            None if launch_metadata.is_empty() => None,
            None => {
                launch_metadata.validate()?;
                Some(launch_metadata.clone())
            }
        };
        if merged_launch_metadata
            .as_ref()
            .and_then(|metadata| metadata.launch_driver)
            == Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
        {
            bail!("in-process handlers cannot attach an external process identity");
        }
        if let Some(existing_identity) = existing_identity {
            if require_empty {
                bail!(
                    "refusing pre-release attachment for thread {thread_id}: process identity is already attached"
                );
            }
            let existing_identity =
                serde_json::from_str::<ExecutionProcessIdentity>(&existing_identity)
                    .context("failed to decode existing process_identity during attach")?;
            if existing_pid != Some(pid)
                || existing_pgid != Some(pgid)
                || existing_identity != *process_identity
            {
                bail!("refusing to replace immutable process identity for thread {thread_id}");
            }
            // Exact repeated self-attach is idempotent. A later trusted
            // in-process attach may enrich metadata that the first UDS attach
            // intentionally left empty, but it cannot change process identity.
            // Once a stop is tombstoned, keep the exact repeat idempotent but do
            // not mutate launch metadata during cancellation.
            if let (None, Some(merged_launch_metadata)) =
                (stop_requested_at_ms, merged_launch_metadata.as_ref())
            {
                let lm_json = encode_current_launch_metadata(merged_launch_metadata)
                    .context("failed to encode launch_metadata")?;
                self.conn.execute(
                    "UPDATE thread_runtime SET launch_metadata = ?2 WHERE thread_id = ?1",
                    params![thread_id, lm_json],
                )?;
            }
            return Ok(());
        }
        if stop_requested_at_ms.is_some() {
            bail!("refusing to attach process to stop-requested thread {thread_id}");
        }
        if existing_pid.is_some() || existing_pgid.is_some() {
            bail!("refusing to attach over unverified pid/pgid residue for thread {thread_id}");
        }

        // Preserve seeded launch metadata. A self-attach over UDS sends only
        // thread/pid, so its `launch_metadata` is the serde default (empty); do
        // NOT let that clobber metadata already seeded on the row at spawn
        // (resume context / continuation spec). Update only pid/pgid in that case.
        let Some(merged_launch_metadata) = merged_launch_metadata else {
            let updated = self.conn.execute(
                "UPDATE thread_runtime
                    SET pid = ?2, pgid = ?3, process_identity = ?4
                  WHERE thread_id = ?1
                    AND pid IS NULL AND pgid IS NULL AND process_identity IS NULL
                    AND stop_requested_at_ms IS NULL",
                params![thread_id, pid, pgid, identity_json],
            )?;
            if updated == 0 {
                bail!("thread_runtime row missing for thread_id: {thread_id}");
            }
            return Ok(());
        };
        let lm_json = encode_current_launch_metadata(&merged_launch_metadata)
            .context("failed to encode launch_metadata")?;
        let updated = self.conn.execute(
            "UPDATE thread_runtime
                SET pid = ?2, pgid = ?3, launch_metadata = ?4, process_identity = ?5
              WHERE thread_id = ?1
                AND pid IS NULL AND pgid IS NULL AND process_identity IS NULL
                AND stop_requested_at_ms IS NULL",
            params![thread_id, pid, pgid, lm_json, identity_json],
        )?;
        if updated == 0 {
            bail!("thread_runtime row missing for thread_id: {thread_id}");
        }
        Ok(())
    }

    /// Atomically close the attach window for an explicit stop request and
    /// return the process identity that was attached before the tombstone.
    /// A concurrent attach is serialized by the StateStore lock: it either
    /// lands first and is returned here, or observes the tombstone and fails.
    pub fn request_thread_stop(&self, thread_id: &str, intent: StopIntent) -> Result<RuntimeInfo> {
        let now_ms = lillux::time::timestamp_millis();
        let updated = self.conn.execute(
            "UPDATE thread_runtime
                SET stop_requested_at_ms = COALESCE(stop_requested_at_ms, ?2),
                    stop_intent = CASE
                        WHEN stop_intent = 'kill' OR ?3 = 'kill' THEN 'kill'
                        ELSE 'cancel'
                    END
              WHERE thread_id = ?1",
            params![thread_id, now_ms, intent.as_str()],
        )?;
        if updated == 0 {
            bail!("thread_runtime row missing for thread_id: {thread_id}");
        }
        self.get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow::anyhow!("thread_runtime row disappeared for {thread_id}"))
    }

    /// Clear live process ownership only if it is still the exact incarnation
    /// the caller finished waiting/reaping. This cannot erase a later attach.
    pub fn clear_process_if_matches(
        &self,
        thread_id: &str,
        process_identity: &ExecutionProcessIdentity,
    ) -> Result<bool> {
        let identity_json = serde_json::to_string(process_identity)
            .context("failed to encode process_identity for compare-and-clear")?;
        Ok(self.conn.execute(
            "UPDATE thread_runtime
                SET pid = NULL, pgid = NULL, process_identity = NULL,
                    process_dead_observed_at_ms = NULL
              WHERE thread_id = ?1 AND process_identity = ?2",
            params![thread_id, identity_json],
        )? > 0)
    }

    /// Persist the first live-sweep observation that one exact attached
    /// process group is gone. Repeated observations return the same timestamp;
    /// a replaced attachment returns `None` and is never affected.
    pub fn observe_dead_process_if_matches(
        &self,
        thread_id: &str,
        process_identity: &ExecutionProcessIdentity,
        observed_at_ms: i64,
    ) -> Result<Option<i64>> {
        let identity_json = serde_json::to_string(process_identity)
            .context("encode process identity for dead-owner observation")?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE thread_runtime
                SET process_dead_observed_at_ms = COALESCE(process_dead_observed_at_ms, ?3)
              WHERE thread_id=?1 AND process_identity=?2",
            params![thread_id, identity_json, observed_at_ms],
        )?;
        let observed = tx
            .query_row(
                "SELECT process_dead_observed_at_ms FROM thread_runtime
                  WHERE thread_id=?1 AND process_identity=?2",
                params![thread_id, identity_json],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        tx.commit()?;
        Ok(observed)
    }

    pub fn list_attached_thread_ids(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT thread_id FROM thread_runtime
              WHERE process_identity IS NOT NULL
              ORDER BY thread_id",
        )?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Seed/overwrite a thread's launch metadata WITHOUT touching pid/pgid. Used
    /// at spawn time to persist the launch identity (resume context /
    /// continuation spec) before the process self-attaches; the
    /// clobber-preserving [`Self::attach_process`] keeps it against a later empty
    /// self-attach.
    pub fn set_launch_metadata(
        &self,
        thread_id: &str,
        launch_metadata: &RuntimeLaunchMetadata,
    ) -> Result<()> {
        if launch_metadata.launch_driver
            == Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
        {
            bail!("in-process launch metadata must be installed by the atomic birth reservation");
        }
        let lm_json = encode_current_launch_metadata(launch_metadata)
            .context("failed to encode launch_metadata")?;
        let updated = self.conn.execute(
            "UPDATE thread_runtime SET launch_metadata = ?2 WHERE thread_id = ?1",
            params![thread_id, lm_json],
        )?;
        if updated == 0 {
            bail!("thread_runtime row missing for thread_id: {thread_id}");
        }
        Ok(())
    }

    pub fn get_runtime_info(&self, thread_id: &str) -> Result<Option<RuntimeInfo>> {
        // Decode loudly outside the rusqlite mapper so we can log the
        // thread_id and raw payload on schema drift. A silent `.ok()`
        // here would disable cancellation routing, resume eligibility
        // and the checkpoint dir on a single corrupt row.
        let raw = self
            .conn
            .query_row(
                "SELECT pid, pgid, launch_metadata, process_identity,
                        process_dead_observed_at_ms, stop_requested_at_ms, stop_intent
                   FROM thread_runtime WHERE thread_id = ?1",
                params![thread_id],
                |row| {
                    let pid: Option<i64> = row.get(0)?;
                    let pgid: Option<i64> = row.get(1)?;
                    let lm_text: Option<String> = row.get(2)?;
                    let identity_text: Option<String> = row.get(3)?;
                    let process_dead_observed_at_ms: Option<i64> = row.get(4)?;
                    let stop_requested_at_ms: Option<i64> = row.get(5)?;
                    let stop_intent: Option<String> = row.get(6)?;
                    Ok((
                        pid,
                        pgid,
                        lm_text,
                        identity_text,
                        process_dead_observed_at_ms,
                        stop_requested_at_ms,
                        stop_intent,
                    ))
                },
            )
            .optional()?;
        let Some((
            pid,
            pgid,
            lm_text,
            identity_text,
            process_dead_observed_at_ms,
            stop_requested_at_ms,
            stop_intent,
        )) = raw
        else {
            return Ok(None);
        };
        let stored_launch_metadata = lm_text
            .as_deref()
            .map(decode_stored_launch_metadata)
            .transpose()
            .with_context(|| {
                format!(
                    "failed to decode launch_metadata for thread {thread_id} (payload_len={})",
                    lm_text.as_deref().map_or(0, str::len)
                )
            })?;
        let (launch_metadata, incompatible_launch_metadata) = match stored_launch_metadata {
            Some(StoredLaunchMetadata::Current(metadata)) => (Some(*metadata), None),
            Some(StoredLaunchMetadata::Incompatible(metadata)) => (None, Some(metadata)),
            None => (None, None),
        };
        let process_identity = match identity_text.as_deref() {
            None => None,
            Some(value) => {
                let identity = serde_json::from_str::<ExecutionProcessIdentity>(value)
                    .with_context(|| {
                        format!(
                            "failed to decode process_identity for thread {thread_id} (payload_len={})",
                            value.len()
                        )
                    })?;
                if identity.schema_version != PROCESS_IDENTITY_SCHEMA_VERSION
                    || Some(identity.target_pid) != pid
                    || Some(identity.group_leader_pid) != pgid
                {
                    bail!(
                        "process_identity mismatch for thread {thread_id}: persisted pid/pgid={pid:?}/{pgid:?}"
                    );
                }
                validate_execution_process_identity_shape(&identity).with_context(|| {
                    format!("invalid process_identity shape for thread {thread_id}")
                })?;
                Some(identity)
            }
        };
        if !matches!(
            (pid, pgid, process_identity.as_ref()),
            (None, None, None) | (Some(_), Some(_), Some(_))
        ) {
            bail!(
                "incomplete process attachment for thread {thread_id}: pid/pgid/identity must be all present or all absent"
            );
        }
        if process_dead_observed_at_ms.is_some() && process_identity.is_none() {
            bail!("thread {thread_id} has a dead-process observation without an attached identity");
        }
        let stop_intent = stop_intent.as_deref().map(StopIntent::parse).transpose()?;
        if stop_requested_at_ms.is_some() != stop_intent.is_some() {
            bail!(
                "incomplete durable stop tombstone for thread {thread_id}: timestamp and intent must be present together"
            );
        }
        let recovery_wait = self.recovery_wait(thread_id)?;
        Ok(Some(RuntimeInfo {
            pid,
            pgid,
            process_identity,
            process_dead_observed_at_ms,
            stop_requested_at_ms,
            stop_intent,
            launch_metadata,
            incompatible_launch_metadata,
            recovery_wait,
        }))
    }

    /// Read the auto-resume attempt counter for a thread.
    /// Missing row (legitimate fresh thread) ⇒ 0.
    /// Row present but `resume_attempts` NULL (corruption) ⇒ bail.
    pub fn get_resume_attempts(&self, thread_id: &str) -> Result<u32> {
        let row_exists: bool = self.conn.query_row(
            "SELECT COUNT(*) > 0 FROM thread_runtime WHERE thread_id = ?1",
            params![thread_id],
            |row| row.get(0),
        )?;
        if !row_exists {
            return Ok(0);
        }
        let n: Option<i64> = self.conn.query_row(
            "SELECT resume_attempts FROM thread_runtime WHERE thread_id = ?1",
            params![thread_id],
            |row| row.get(0),
        )?;
        match n {
            Some(v) => {
                if v < 0 {
                    bail!(
                        "resume_attempts is negative ({v}) for thread {thread_id} — \
                         corrupt row; refusing to fabricate a counter"
                    );
                }
                Ok(v as u32)
            }
            None => bail!(
                "resume_attempts is NULL for thread {thread_id} — \
                 corrupt row; refusing to fabricate a counter"
            ),
        }
    }

    /// Atomically increment the auto-resume attempt counter for a
    /// thread and return the post-increment value. Used by
    /// `reconcile.rs` BEFORE re-spawning so a crash mid-resume does
    /// not grant an infinite retry loop.
    #[tracing::instrument(
        name = "state:resume_attempts_bump",
        skip(self),
        fields(thread_id = %thread_id, attempt = tracing::field::Empty)
    )]
    pub fn bump_resume_attempts(&self, thread_id: &str) -> Result<u32> {
        let updated = self.conn.execute(
            "UPDATE thread_runtime
                SET resume_attempts = resume_attempts + 1
              WHERE thread_id = ?1",
            params![thread_id],
        )?;
        if updated == 0 {
            bail!("thread_runtime row missing for thread_id: {thread_id}");
        }
        self.get_resume_attempts(thread_id)
    }

    /// Atomically claim the right to launch `thread_id`, returning whether the
    /// caller won the claim.
    ///
    /// This is the sole authorization for a spawn. A fresh thread takes the
    /// claim; a thread already mid-launch returns
    /// [`LaunchClaimOutcome::AlreadyClaimed`]. Claims deliberately do not expire
    /// within a daemon lifetime: pre-attach resolution and materialization are
    /// unbounded, so a wall-clock lease cannot be the sole spawn authorization.
    /// Owned guards release on every task exit, and startup clears all surviving
    /// rows after the state lock proves the previous daemon is gone.
    pub fn claim_thread_launch(
        &self,
        thread_id: &str,
        claim_id: &str,
        daemon_generation_id: &str,
    ) -> Result<LaunchClaimOutcome> {
        let now_ms = lillux::time::timestamp_millis();
        let tx = self.conn.unchecked_transaction()?;
        let already_claimed: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM thread_launch_claim WHERE thread_id=?1)",
            params![thread_id],
            |row| row.get(0),
        )?;
        if already_claimed {
            tx.rollback()?;
            return Ok(LaunchClaimOutcome::AlreadyClaimed);
        }
        let next_epoch: i64 = tx.query_row(
            "INSERT INTO thread_launch_epoch(thread_id,last_epoch) VALUES (?1,1)
             ON CONFLICT(thread_id) DO UPDATE SET last_epoch=last_epoch+1
             RETURNING last_epoch",
            params![thread_id],
            |row| row.get(0),
        )?;
        let owner = LaunchOwner {
            thread_id: thread_id.to_string(),
            monotonic_launch_epoch: u64::try_from(next_epoch)
                .context("launch epoch cannot be represented")?,
            unpredictable_nonce: claim_id.to_string(),
            daemon_generation_id: daemon_generation_id.to_string(),
        };
        let owner_json = lillux::canonical_json(&serde_json::to_value(&owner)?)?;
        let changed = tx.execute(
            "INSERT INTO thread_launch_claim
                 (thread_id, claim_id, claimed_at_ms, lease_expires_at_ms, claimed_by)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(thread_id) DO NOTHING",
            params![thread_id, claim_id, now_ms, i64::MAX, owner_json],
        )?;
        if changed != 1 {
            tx.rollback()?;
            return Ok(LaunchClaimOutcome::AlreadyClaimed);
        }
        tx.commit()?;
        Ok(LaunchClaimOutcome::Claimed)
    }

    /// Clear every launch claim owned by a daemon generation other than
    /// `current_daemon_generation_id` — the startup half of the claim
    /// contract. Claims deliberately never expire by wall clock, on the
    /// promise that a restart clears the previous daemon's survivors; a claim
    /// from another generation cannot have a live launch task by
    /// construction, and leaving it would turn every future recovery of its
    /// thread into a silent `AlreadyClaimed` skip — the stranded-`created`
    /// bug. A row whose stored owner does not parse is equally dead (this
    /// generation only writes valid owners) and is cleared fail-closed.
    /// Deletes are exact `(thread_id, claim_id)` matches, never cross-owner.
    pub fn clear_stale_launch_claims(
        &self,
        current_daemon_generation_id: &str,
    ) -> Result<Vec<StaleLaunchClaimCleared>> {
        let mut rows: Vec<(String, String, String)> = Vec::new();
        {
            let mut statement = self
                .conn
                .prepare("SELECT thread_id, claim_id, claimed_by FROM thread_launch_claim")?;
            let mapped =
                statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            for row in mapped {
                rows.push(row?);
            }
        }
        let mut cleared = Vec::new();
        for (thread_id, claim_id, claimed_by) in rows {
            let dead_generation = match serde_json::from_str::<LaunchOwner>(&claimed_by) {
                Ok(owner) if owner.daemon_generation_id == current_daemon_generation_id => {
                    continue;
                }
                Ok(owner) => owner.daemon_generation_id,
                Err(_) => "<malformed owner>".to_string(),
            };
            let removed = self.conn.execute(
                "DELETE FROM thread_launch_claim WHERE thread_id = ?1 AND claim_id = ?2",
                params![thread_id, claim_id],
            )?;
            if removed > 0 {
                cleared.push(StaleLaunchClaimCleared {
                    thread_id,
                    claim_id,
                    dead_generation,
                });
            }
        }
        Ok(cleared)
    }

    /// Release a launch claim the caller owns (matched by `claim_id`), e.g. when
    /// the launch failed and the thread should become reclaimable immediately
    /// rather than waiting for restart recovery. Returns true if a row was
    /// removed. A mismatched `claim_id` is a no-op, never a cross-owner delete.
    pub fn release_thread_launch_claim(&self, thread_id: &str, claim_id: &str) -> Result<bool> {
        let removed = self.conn.execute(
            "DELETE FROM thread_launch_claim WHERE thread_id = ?1 AND claim_id = ?2",
            params![thread_id, claim_id],
        )?;
        Ok(removed > 0)
    }

    /// Read the current launch claim for a thread, if any. The reconciler uses
    /// this to tell an unlaunched successor from one owned by a launch task.
    pub fn get_launch_claim(&self, thread_id: &str) -> Result<Option<LaunchClaim>> {
        let claim = self
            .conn
            .query_row(
                "SELECT thread_id, claim_id, claimed_at_ms, lease_expires_at_ms, claimed_by
                   FROM thread_launch_claim WHERE thread_id = ?1",
                params![thread_id],
                |row| {
                    let claimed_by: String = row.get(4)?;
                    let owner = serde_json::from_str(&claimed_by).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            4,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?;
                    Ok(LaunchClaim {
                        thread_id: row.get(0)?,
                        claim_id: row.get(1)?,
                        claimed_at_ms: row.get(2)?,
                        lease_expires_at_ms: row.get(3)?,
                        claimed_by,
                        owner,
                    })
                },
            )
            .optional()
            .map_err(anyhow::Error::from)?;
        if let Some(claim) = claim.as_ref() {
            let canonical_owner = lillux::canonical_json(&serde_json::to_value(&claim.owner)?)?;
            let persisted_epoch: i64 = self.conn.query_row(
                "SELECT last_epoch FROM thread_launch_epoch WHERE thread_id=?1",
                params![&claim.thread_id],
                |row| row.get(0),
            )?;
            if claim.owner.thread_id != claim.thread_id
                || claim.owner.unpredictable_nonce != claim.claim_id
                || claim.owner.monotonic_launch_epoch == 0
                || claim.owner.daemon_generation_id.is_empty()
                || claim.owner.monotonic_launch_epoch != u64::try_from(persisted_epoch)?
                || canonical_owner != claim.claimed_by
            {
                bail!("durable launch owner fields contradict their claim row");
            }
        }
        Ok(claim)
    }

    pub fn reserve_workspace(
        &self,
        workspace_id: &str,
        lower_snapshot: &str,
        root_path: &str,
    ) -> Result<()> {
        let now = lillux::time::timestamp_millis();
        let changed = self.conn.execute(
            "INSERT INTO execution_workspace
                (workspace_id, lower_snapshot, root_path, state, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(workspace_id) DO NOTHING",
            params![
                workspace_id,
                lower_snapshot,
                root_path,
                WorkspaceState::Reserved.as_str(),
                now
            ],
        )?;
        if changed == 0 {
            let existing = self
                .workspace(workspace_id)?
                .ok_or_else(|| anyhow::anyhow!("workspace reservation disappeared"))?;
            if existing.lower_snapshot != lower_snapshot
                || existing.root_path != root_path
                || !matches!(
                    existing.state,
                    WorkspaceState::Reserved | WorkspaceState::Constructing
                )
                || existing.process_identity.is_some()
                || (existing.state == WorkspaceState::Reserved
                    && (existing.thread_id.is_some() || existing.launch_owner.is_some()))
            {
                bail!("workspace {workspace_id} cannot adopt a conflicting durable reservation");
            }
        }
        Ok(())
    }

    pub fn claim_workspace_construction(
        &self,
        workspace_id: &str,
        thread_id: &str,
        launch_owner: &str,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE execution_workspace
                SET thread_id=?2, launch_owner=?3, updated_at_ms=?4
              WHERE workspace_id=?1 AND thread_id IS NULL AND launch_owner IS NULL
                AND state=?5",
            params![
                workspace_id,
                thread_id,
                launch_owner,
                lillux::time::timestamp_millis(),
                WorkspaceState::Constructing.as_str()
            ],
        )?;
        if changed != 1 {
            let existing = self
                .workspace(workspace_id)?
                .ok_or_else(|| anyhow::anyhow!("workspace {workspace_id} disappeared"))?;
            if existing.thread_id.as_deref() != Some(thread_id)
                || existing.launch_owner.as_deref() != Some(launch_owner)
                || existing.state != WorkspaceState::Constructing
            {
                bail!("workspace {workspace_id} cannot claim construction ownership");
            }
        }
        Ok(())
    }

    /// Persist the exact signed adapter selection before invoking any
    /// backend lifecycle operation. A crash after this point can only be
    /// reconciled by the same backend build; recovery never guesses which
    /// implementation may have created external workspace state.
    pub fn prepare_workspace_backend(
        &self,
        workspace_id: &str,
        thread_id: &str,
        launch_owner: &str,
        backend_id: &str,
        backend_version: &str,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE execution_workspace
                SET backend_id=?4, backend_version=?5, updated_at_ms=?6
              WHERE workspace_id=?1 AND thread_id=?2 AND launch_owner=?3
                AND state=?7
                AND (backend_id IS NULL OR backend_id=?4)
                AND (backend_version IS NULL OR backend_version=?5)",
            params![
                workspace_id,
                thread_id,
                launch_owner,
                backend_id,
                backend_version,
                lillux::time::timestamp_millis(),
                WorkspaceState::Constructing.as_str()
            ],
        )?;
        if changed != 1 {
            let existing = self
                .workspace(workspace_id)?
                .ok_or_else(|| anyhow::anyhow!("workspace {workspace_id} disappeared"))?;
            if existing.thread_id.as_deref() != Some(thread_id)
                || existing.launch_owner.as_deref() != Some(launch_owner)
                || existing.backend_id.as_deref() != Some(backend_id)
                || existing.backend_version.as_deref() != Some(backend_version)
                || existing.state != WorkspaceState::Constructing
            {
                bail!(
                    "workspace {workspace_id} cannot record the selected backend before construction"
                );
            }
        }
        Ok(())
    }

    pub fn bind_workspace(&self, binding: WorkspaceBinding<'_>) -> Result<()> {
        let WorkspaceBinding {
            workspace_id,
            thread_id,
            launch_owner,
            backend_id,
            backend_version,
            pinned_root_identities,
            mount_identity,
        } = binding;
        let changed = self.conn.execute(
            "UPDATE execution_workspace
                SET backend_id=?4,
                    backend_version=?5, pinned_root_identities=?6, mount_identity=?7,
                    state=?8, updated_at_ms=?9
              WHERE workspace_id=?1 AND thread_id=?2 AND launch_owner=?3
                AND state=?10",
            params![
                workspace_id,
                thread_id,
                launch_owner,
                backend_id,
                backend_version,
                pinned_root_identities,
                mount_identity,
                WorkspaceState::Ready.as_str(),
                lillux::time::timestamp_millis(),
                WorkspaceState::Constructing.as_str()
            ],
        )?;
        if changed != 1 {
            let existing = self
                .workspace(workspace_id)?
                .ok_or_else(|| anyhow::anyhow!("workspace {workspace_id} disappeared"))?;
            if existing.thread_id.as_deref() != Some(thread_id)
                || existing.launch_owner.as_deref() != launch_owner
                || existing.backend_id.as_deref() != backend_id
                || existing.backend_version.as_deref() != backend_version
                || existing.pinned_root_identities.as_deref() != pinned_root_identities
                || existing.mount_identity.as_deref() != mount_identity
                || existing.state != WorkspaceState::Ready
            {
                bail!("workspace {workspace_id} cannot be bound from its current state");
            }
        }
        Ok(())
    }

    pub fn transition_workspace(
        &self,
        workspace_id: &str,
        expected: &[WorkspaceState],
        next: WorkspaceState,
        process_identity: Option<&str>,
    ) -> Result<()> {
        if expected.is_empty() {
            bail!("workspace transition requires an expected state");
        }
        let placeholders = std::iter::repeat_n("?", expected.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "UPDATE execution_workspace
                SET state=?2, process_identity=COALESCE(?3, process_identity), updated_at_ms=?4
              WHERE workspace_id=?1 AND state IN ({placeholders})"
        );
        let now = lillux::time::timestamp_millis();
        let mut values: Vec<rusqlite::types::Value> = vec![
            workspace_id.to_owned().into(),
            next.as_str().to_owned().into(),
            process_identity.map_or(rusqlite::types::Value::Null, |value| {
                value.to_owned().into()
            }),
            now.into(),
        ];
        values.extend(
            expected
                .iter()
                .map(|value| rusqlite::types::Value::from(value.as_str().to_owned())),
        );
        let changed = self
            .conn
            .execute(&sql, rusqlite::params_from_iter(values))?;
        if changed != 1 {
            bail!("workspace {workspace_id} cannot transition to {next}");
        }
        Ok(())
    }

    pub fn transition_workspace_owned(
        &self,
        workspace_id: &str,
        thread_id: &str,
        launch_owner: &str,
        expected: &[WorkspaceState],
        next: WorkspaceState,
        process_identity: Option<&str>,
    ) -> Result<()> {
        if expected.is_empty() {
            bail!("workspace transition requires an expected state");
        }
        let placeholders = (7..7 + expected.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "UPDATE execution_workspace
                SET state=?2, process_identity=COALESCE(?3, process_identity), updated_at_ms=?4
              WHERE workspace_id=?1 AND thread_id=?5 AND launch_owner=?6
                AND state IN ({placeholders})"
        );
        let mut values: Vec<rusqlite::types::Value> = vec![
            workspace_id.to_owned().into(),
            next.as_str().to_owned().into(),
            process_identity.map_or(rusqlite::types::Value::Null, |value| {
                value.to_owned().into()
            }),
            lillux::time::timestamp_millis().into(),
            thread_id.to_owned().into(),
            launch_owner.to_owned().into(),
        ];
        values.extend(
            expected
                .iter()
                .map(|value| rusqlite::types::Value::from(value.as_str().to_owned())),
        );
        let changed = self
            .conn
            .execute(&sql, rusqlite::params_from_iter(values))?;
        if changed != 1 {
            bail!("stale launch owner cannot transition workspace {workspace_id} to {next}");
        }
        Ok(())
    }

    /// Transfer a retained workspace from one proved-dead launch owner to the
    /// current same-thread recovery claim. Backend/root evidence is verified
    /// by the caller before this transaction; this boundary makes the owner
    /// replacement and removal of the stale process attachment indivisible.
    pub fn rebind_workspace_for_recovery(
        &self,
        workspace_id: &str,
        thread_id: &str,
        previous_launch_owner: &str,
        recovery_launch_owner: &str,
        expected_state: WorkspaceState,
        expected_process_identity: Option<&str>,
    ) -> Result<()> {
        if !matches!(
            expected_state,
            WorkspaceState::Ready | WorkspaceState::Active | WorkspaceState::Freezing
        ) {
            bail!("retained workspace recovery requires ready, active, or freezing state");
        }
        let recovery_state = if expected_state == WorkspaceState::Freezing {
            WorkspaceState::Freezing
        } else {
            WorkspaceState::Ready
        };
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .context("begin retained workspace owner transfer")?;
        let claim_owner: Option<String> = tx
            .query_row(
                "SELECT claimed_by FROM thread_launch_claim WHERE thread_id=?1",
                params![thread_id],
                |row| row.get(0),
            )
            .optional()?;
        if claim_owner.as_deref() != Some(recovery_launch_owner) {
            bail!("retained workspace recovery lost its current launch claim");
        }
        let existing: Option<(
            Option<String>,
            Option<String>,
            WorkspaceState,
            Option<String>,
        )> = tx
            .query_row(
                "SELECT thread_id, launch_owner, state, process_identity
                   FROM execution_workspace WHERE workspace_id=?1",
                params![workspace_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((workspace_thread, workspace_owner, state, process_identity)) = existing else {
            bail!("retained workspace {workspace_id} disappeared");
        };
        if workspace_thread.as_deref() != Some(thread_id)
            || workspace_owner.as_deref() != Some(previous_launch_owner)
            || state != expected_state
            || process_identity.as_deref() != expected_process_identity
        {
            bail!("retained workspace owner or process evidence changed before recovery");
        }
        let changed = tx.execute(
            "UPDATE execution_workspace
                SET launch_owner=?4, state=?5, process_identity=NULL, updated_at_ms=?6
              WHERE workspace_id=?1 AND thread_id=?2 AND launch_owner=?3
                AND state=?7
                AND ((?8 IS NULL AND process_identity IS NULL) OR process_identity=?8)",
            params![
                workspace_id,
                thread_id,
                previous_launch_owner,
                recovery_launch_owner,
                recovery_state.as_str(),
                lillux::time::timestamp_millis(),
                expected_state.as_str(),
                expected_process_identity,
            ],
        )?;
        if changed != 1 {
            bail!("retained workspace owner transfer lost its exact row CAS");
        }
        tx.commit()
            .context("commit retained workspace owner transfer")
    }

    /// Atomically publish the result of a callback freeze into both durable
    /// recovery authorities. The workspace row is the lifecycle journal; the
    /// thread launch metadata is the native-resume seed. They must never name
    /// different generations after a crash.
    pub fn bind_frozen_workspace_generation(
        &self,
        workspace_id: &str,
        thread_id: &str,
        launch_owner: &str,
        snapshot_hash: &str,
    ) -> Result<()> {
        validate_sha256("frozen workspace snapshot hash", snapshot_hash)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .context("begin atomic frozen workspace binding")?;
        let claim_owner: Option<String> = tx
            .query_row(
                "SELECT claimed_by FROM thread_launch_claim WHERE thread_id=?1",
                params![thread_id],
                |row| row.get(0),
            )
            .optional()?;
        if claim_owner.as_deref() != Some(launch_owner) {
            bail!("stale launch owner cannot bind frozen workspace {workspace_id}");
        }
        let (workspace_thread, workspace_owner, state, lower_snapshot, existing_frozen): (
            Option<String>,
            Option<String>,
            WorkspaceState,
            String,
            Option<String>,
        ) = tx
            .query_row(
                "SELECT thread_id, launch_owner, state, lower_snapshot, frozen_snapshot_hash
                   FROM execution_workspace WHERE workspace_id=?1",
                params![workspace_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("workspace {workspace_id} disappeared"))?;
        if workspace_thread.as_deref() != Some(thread_id)
            || workspace_owner.as_deref() != Some(launch_owner)
            || state != WorkspaceState::Freezing
            || existing_frozen
                .as_deref()
                .is_some_and(|existing| existing != snapshot_hash)
        {
            bail!("workspace {workspace_id} cannot bind a conflicting frozen generation");
        }
        let launch_metadata_json: Option<String> = tx
            .query_row(
                "SELECT launch_metadata FROM thread_runtime WHERE thread_id=?1",
                params![thread_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("thread_runtime row missing for {thread_id}"))?;
        let launch_metadata_json = launch_metadata_json.ok_or_else(|| {
            anyhow::anyhow!("thread {thread_id} has no launch metadata for frozen workspace")
        })?;
        let launch_metadata = decode_current_launch_metadata(&launch_metadata_json)
            .context("decode launch metadata while binding frozen workspace")?;
        let admitted_project_authority = launch_metadata
            .admitted_project_authority
            .as_ref()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "thread {thread_id} has no admitted project authority for frozen workspace"
                )
            })?;
        if admitted_project_authority.operational_snapshot_projection()
            != Some(lower_snapshot.as_str())
        {
            bail!("workspace lower snapshot and admitted launch authority contradict");
        }
        let workspace_updated = tx.execute(
            "UPDATE execution_workspace
                SET frozen_snapshot_hash=?4, updated_at_ms=?5
              WHERE workspace_id=?1 AND thread_id=?2 AND launch_owner=?3
                AND state=?6
                AND (frozen_snapshot_hash IS NULL OR frozen_snapshot_hash=?4)",
            params![
                workspace_id,
                thread_id,
                launch_owner,
                snapshot_hash,
                lillux::time::timestamp_millis(),
                WorkspaceState::Freezing.as_str()
            ],
        )?;
        if workspace_updated != 1 {
            bail!("frozen workspace binding lost its durable owner row");
        }
        tx.commit()
            .context("commit atomic frozen workspace binding")
    }

    pub fn workspace(&self, workspace_id: &str) -> Result<Option<WorkspaceRecord>> {
        self.conn
            .query_row(
                "SELECT workspace_id, thread_id, launch_owner, backend_id, backend_version,
                        pinned_root_identities, mount_identity, lower_snapshot,
                        frozen_snapshot_hash, root_path, state, process_identity, created_at_ms, updated_at_ms
                   FROM execution_workspace WHERE workspace_id=?1",
                params![workspace_id],
                |row| {
                    Ok(WorkspaceRecord {
                        workspace_id: row.get(0)?,
                        thread_id: row.get(1)?,
                        launch_owner: row.get(2)?,
                        backend_id: row.get(3)?,
                        backend_version: row.get(4)?,
                        pinned_root_identities: row.get(5)?,
                        mount_identity: row.get(6)?,
                        lower_snapshot: row.get(7)?,
                        frozen_snapshot_hash: row.get(8)?,
                        root_path: row.get(9)?,
                        state: row.get(10)?,
                        process_identity: row.get(11)?,
                        created_at_ms: row.get(12)?,
                        updated_at_ms: row.get(13)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn open_workspaces(&self) -> Result<Vec<WorkspaceRecord>> {
        let mut statement = self.conn.prepare(
            "SELECT workspace_id, thread_id, launch_owner, backend_id, backend_version,
                    pinned_root_identities, mount_identity, lower_snapshot,
                    frozen_snapshot_hash, root_path, state, process_identity, created_at_ms, updated_at_ms
               FROM execution_workspace WHERE state != ?1 ORDER BY created_at_ms",
        )?;
        let rows = statement.query_map(params![WorkspaceState::Closed.as_str()], |row| {
            Ok(WorkspaceRecord {
                workspace_id: row.get(0)?,
                thread_id: row.get(1)?,
                launch_owner: row.get(2)?,
                backend_id: row.get(3)?,
                backend_version: row.get(4)?,
                pinned_root_identities: row.get(5)?,
                mount_identity: row.get(6)?,
                lower_snapshot: row.get(7)?,
                frozen_snapshot_hash: row.get(8)?,
                root_path: row.get(9)?,
                state: row.get(10)?,
                process_identity: row.get(11)?,
                created_at_ms: row.get(12)?,
                updated_at_ms: row.get(13)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn workspace_for_thread(&self, thread_id: &str) -> Result<Option<WorkspaceRecord>> {
        validate_bounded_runtime_text("workspace thread id", thread_id, 256)?;
        let matches = self
            .open_workspaces()?
            .into_iter()
            .filter(|workspace| workspace.thread_id.as_deref() == Some(thread_id))
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            bail!("thread owns more than one open execution workspace");
        }
        Ok(matches.into_iter().next())
    }

    pub fn submit_command(&self, cmd: &NewCommandRecord) -> Result<CommandRecord> {
        validate_command_type(&cmd.command_type)?;
        let now = now_rfc3339();
        if cmd
            .requested_by
            .as_ref()
            .is_some_and(|requested_by| requested_by.len() > MAX_COMMAND_REQUESTED_BY_BYTES)
        {
            bail!("command requested_by exceeds the {MAX_COMMAND_REQUESTED_BY_BYTES}-byte maximum");
        }
        let params_blob = json_blob(&cmd.params)?;
        let params_bytes = params_blob.as_ref().map_or(0, Vec::len);
        if params_bytes > MAX_COMMAND_PARAMS_BYTES {
            bail!("command params are {params_bytes} bytes; maximum is {MAX_COMMAND_PARAMS_BYTES}");
        }
        let requested_by_bytes = cmd.requested_by.as_ref().map_or(0, String::len);
        let candidate_content_bytes = cmd
            .command_type
            .len()
            .checked_add(requested_by_bytes)
            .and_then(|bytes| bytes.checked_add(params_bytes))
            .context("command content size overflow")?;
        let transaction = self.conn.unchecked_transaction()?;
        let (open_items, open_content_bytes): (i64, i64) = transaction.query_row(
            "SELECT COUNT(*), \
                    COALESCE(SUM(length(CAST(command_type AS BLOB)) + \
                                 COALESCE(length(CAST(requested_by AS BLOB)), 0) + \
                                 COALESCE(length(params), 0) + COALESCE(length(result), 0)), 0) \
             FROM thread_commands \
             WHERE thread_id = ?1 AND status IN ('pending', 'claimed')",
            params![&cmd.thread_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let open_items = usize::try_from(open_items).context("open command count is invalid")?;
        let open_content_bytes =
            usize::try_from(open_content_bytes).context("open command content total is invalid")?;
        if open_items >= MAX_OPEN_COMMANDS_PER_THREAD {
            bail!(
                "thread {} already has {open_items} open commands; maximum is {MAX_OPEN_COMMANDS_PER_THREAD}",
                cmd.thread_id
            );
        }
        let final_content_bytes = open_content_bytes
            .checked_add(candidate_content_bytes)
            .context("open command content total overflow")?;
        if final_content_bytes > MAX_OPEN_COMMAND_CONTENT_BYTES {
            bail!(
                "thread {} open command content would total {final_content_bytes} bytes; maximum is {MAX_OPEN_COMMAND_CONTENT_BYTES}",
                cmd.thread_id
            );
        }
        transaction.execute(
            "INSERT INTO thread_commands (
                thread_id, command_type, status, requested_by, params, result,
                created_at, claimed_at, completed_at
             ) VALUES (?1, ?2, 'pending', ?3, ?4, NULL, ?5, NULL, NULL)",
            params![
                &cmd.thread_id,
                &cmd.command_type,
                &cmd.requested_by,
                params_blob,
                now,
            ],
        )?;
        let command_id = transaction.last_insert_rowid();
        transaction.commit()?;
        self.load_command(command_id)
    }

    pub fn claim_commands(
        &self,
        thread_id: &str,
        limit: usize,
        max_serialized_bytes: usize,
    ) -> Result<Vec<CommandRecord>> {
        if limit == 0 || max_serialized_bytes < b"{\"commands\":[]}".len() {
            bail!("command claim requires a positive item and response budget");
        }
        let limit = limit.min(MAX_COMMAND_CLAIM_ITEMS);
        let max_serialized_bytes = max_serialized_bytes.min(MAX_COMMAND_CLAIM_RESPONSE_BYTES);
        let now = now_rfc3339();
        let transaction = self.conn.unchecked_transaction()?;
        let mut commands = Vec::new();
        let mut response_bytes = b"{\"commands\":[]}".len();
        {
            let sql = format!(
                "{BOUNDED_COMMAND_SELECT} \
                 WHERE thread_id = ?4 AND status = 'pending' \
                 ORDER BY command_id ASC LIMIT ?5"
            );
            let mut stmt = transaction.prepare(&sql)?;
            let sql_limit = i64::try_from(limit).unwrap_or(i64::MAX);
            let rows = stmt.query_map(
                params![
                    i64::try_from(MAX_COMMAND_REQUESTED_BY_BYTES).unwrap_or(i64::MAX),
                    i64::try_from(MAX_COMMAND_PARAMS_BYTES).unwrap_or(i64::MAX),
                    i64::try_from(MAX_COMMAND_RESULT_BYTES).unwrap_or(i64::MAX),
                    thread_id,
                    sql_limit,
                ],
                read_bounded_command_row,
            )?;
            for row in rows {
                let mut command = row?;
                command.status = "claimed".to_string();
                command.claimed_at = Some(now.clone());
                let encoded =
                    serde_json::to_vec(&command).context("failed to size command claim record")?;
                let candidate_bytes = response_bytes
                    .checked_add(encoded.len())
                    .and_then(|bytes| bytes.checked_add(usize::from(!commands.is_empty())))
                    .context("command claim response size overflow")?;
                if candidate_bytes > max_serialized_bytes {
                    if commands.is_empty() {
                        bail!(
                            "pending command {} exceeds claim response budget {}",
                            command.command_id,
                            max_serialized_bytes
                        );
                    }
                    break;
                }
                response_bytes = candidate_bytes;
                commands.push(command);
            }
        }
        for command in &commands {
            let updated = transaction.execute(
                "UPDATE thread_commands
                 SET status = 'claimed', claimed_at = ?2
                 WHERE command_id = ?1 AND status = 'pending'",
                params![command.command_id, &now],
            )?;
            if updated != 1 {
                bail!(
                    "pending command {} changed during claim",
                    command.command_id
                );
            }
        }
        transaction.commit()?;
        Ok(commands)
    }

    pub fn complete_command(
        &self,
        command_id: i64,
        status: &str,
        result: Option<&Value>,
    ) -> Result<CommandRecord> {
        let result_blob = json_blob_ref(result)?;
        let result_bytes = result_blob.as_ref().map_or(0, Vec::len);
        if result_bytes > MAX_COMMAND_RESULT_BYTES {
            bail!("command result is {result_bytes} bytes; maximum is {MAX_COMMAND_RESULT_BYTES}");
        }
        let updated = self.conn.execute(
            "UPDATE thread_commands
             SET status = ?2,
                 result = ?3,
                 completed_at = ?4
             WHERE command_id = ?1 AND status IN ('pending', 'claimed')",
            params![command_id, status, result_blob, now_rfc3339()],
        )?;
        if updated == 0 {
            bail!("command not claimable/completable: {command_id}");
        }

        self.load_command(command_id)
    }

    fn load_command(&self, command_id: i64) -> Result<CommandRecord> {
        self.get_command(command_id)?
            .ok_or_else(|| anyhow::anyhow!("command missing from runtime db: {command_id}"))
    }

    /// Settle every still-open (`pending`/`claimed`) command for a finalized
    /// thread and return the affected records so a waiter blocked in
    /// `commands.wait` is woken instead of riding to its timeout. A command whose
    /// intent the terminal fulfilled — `cancel` for a `cancelled` thread, `kill`
    /// for a `killed` one — settles `completed` (the action took effect); any
    /// other open command settles `rejected` (the thread ended before it was
    /// handled). Each `UPDATE` is guarded on the still-open status, so a row a
    /// runtime completed in the interim is left at its real terminal status.
    pub fn settle_open_commands(
        &self,
        thread_id: &str,
        terminal_status: &str,
    ) -> Result<Vec<CommandRecord>> {
        let transaction = self.conn.unchecked_transaction()?;
        let (open_items, open_content_bytes): (i64, i64) = transaction.query_row(
            "SELECT COUNT(*), \
                    COALESCE(SUM(length(CAST(command_type AS BLOB)) + \
                                 COALESCE(length(CAST(requested_by AS BLOB)), 0) + \
                                 COALESCE(length(params), 0) + COALESCE(length(result), 0)), 0) \
             FROM thread_commands \
             WHERE thread_id = ?1 AND status IN ('pending', 'claimed')",
            params![thread_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let open_items = usize::try_from(open_items).context("open command count is invalid")?;
        let open_content_bytes =
            usize::try_from(open_content_bytes).context("open command content total is invalid")?;
        if open_items > MAX_OPEN_COMMANDS_PER_THREAD {
            bail!(
                "thread {thread_id} has {open_items} open commands; maximum is {MAX_OPEN_COMMANDS_PER_THREAD}"
            );
        }
        if open_content_bytes > MAX_OPEN_COMMAND_CONTENT_BYTES {
            bail!(
                "thread {thread_id} open command content is {open_content_bytes} bytes; maximum is {MAX_OPEN_COMMAND_CONTENT_BYTES}"
            );
        }
        let open: Vec<CommandRecord> = {
            let sql = format!(
                "{BOUNDED_COMMAND_SELECT} \
                 WHERE thread_id = ?4 AND status IN ('pending', 'claimed') \
                 ORDER BY command_id ASC LIMIT ?5"
            );
            let mut stmt = transaction.prepare(&sql)?;
            // Preserve statement/query temporary drop order under Edition 2024.
            #[allow(clippy::let_and_return)]
            let rows = stmt
                .query_map(
                    params![
                        i64::try_from(MAX_COMMAND_REQUESTED_BY_BYTES).unwrap_or(i64::MAX),
                        i64::try_from(MAX_COMMAND_PARAMS_BYTES).unwrap_or(i64::MAX),
                        i64::try_from(MAX_COMMAND_RESULT_BYTES).unwrap_or(i64::MAX),
                        thread_id,
                        i64::try_from(MAX_OPEN_COMMANDS_PER_THREAD + 1).unwrap_or(i64::MAX)
                    ],
                    read_bounded_command_row,
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        if open.len() > MAX_OPEN_COMMANDS_PER_THREAD {
            bail!(
                "thread {thread_id} open command set changed beyond the {MAX_OPEN_COMMANDS_PER_THREAD}-item maximum"
            );
        }

        // Materialize and bound every generated result before the first write.
        // This makes an oversized terminal-status diagnostic fail closed without
        // leaving an earlier command settled and a later one open.
        let mut settlements = Vec::with_capacity(open.len());
        for command in open {
            validate_command_type(&command.command_type).with_context(|| {
                format!(
                    "command {} has an invalid durable command_type",
                    command.command_id
                )
            })?;
            let fulfilled = command_fulfilled_by_terminal(&command.command_type, terminal_status);
            let status = if fulfilled { "completed" } else { "rejected" };
            let result = serde_json::json!({
                "reason": if fulfilled {
                    format!(
                        "thread settled {terminal_status}, fulfilling the {} command",
                        command.command_type
                    )
                } else {
                    format!(
                        "thread finalized ({terminal_status}) before the {} command was handled",
                        command.command_type
                    )
                }
            });
            let result_blob = serde_json::to_vec(&result)
                .context("failed to encode command settlement result")?;
            if result_blob.len() > MAX_COMMAND_RESULT_BYTES {
                bail!(
                    "command {} settlement result is {} bytes; maximum is {MAX_COMMAND_RESULT_BYTES}",
                    command.command_id,
                    result_blob.len()
                );
            }
            settlements.push((command, status, result, result_blob));
        }

        let now = now_rfc3339();
        let mut settled = Vec::with_capacity(settlements.len());
        for (mut command, status, result, result_blob) in settlements {
            let updated = transaction.execute(
                "UPDATE thread_commands SET status = ?2, result = ?3, completed_at = ?4
                 WHERE command_id = ?1 AND status IN ('pending', 'claimed')",
                params![command.command_id, status, result_blob, &now],
            )?;
            if updated > 0 {
                command.status = status.to_string();
                command.result = Some(result);
                command.completed_at = Some(now.clone());
                settled.push(command);
            }
        }
        transaction.commit()?;
        Ok(settled)
    }

    /// Whether a `kill` command was ever submitted for `thread_id`. The
    /// launcher's abnormal-exit fallback uses this as the kill-intent marker: a
    /// subprocess SIGKILLed by a daemon-issued `kill` exits with no callback
    /// finalization (which otherwise normalizes to `failed`); a recorded kill
    /// distinguishes that intentional stop from a genuine crash so it settles
    /// `killed`.
    pub fn thread_has_kill_command(&self, thread_id: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM thread_commands WHERE thread_id = ?1 AND command_type = 'kill'",
            params![thread_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Read one command by id, or `None` if it does not exist. Unlike
    /// `Self::load_command` this is not an error on absence — `commands.get`
    /// and `commands.wait` distinguish "no such command" from a real row.
    pub fn get_command(&self, command_id: i64) -> Result<Option<CommandRecord>> {
        let sql = format!("{BOUNDED_COMMAND_SELECT} WHERE command_id = ?4");
        Ok(self
            .conn
            .query_row(
                &sql,
                params![
                    i64::try_from(MAX_COMMAND_REQUESTED_BY_BYTES).unwrap_or(i64::MAX),
                    i64::try_from(MAX_COMMAND_PARAMS_BYTES).unwrap_or(i64::MAX),
                    i64::try_from(MAX_COMMAND_RESULT_BYTES).unwrap_or(i64::MAX),
                    command_id,
                ],
                read_bounded_command_row,
            )
            .optional()?)
    }

    // ── Child links ──────────────────────────────────────────────────────
    //
    // Operational lineage: which threads a parent spawned (inline dispatch,
    // follow child, …), kept distinct from `follow_waiter` (follow-specific
    // resume state) and the projection (portable history). It exists so a
    // cancel/kill can cascade to a blocked parent's live descendants — a blocked
    // parent cannot claim its own commands, and inline children are fresh
    // projection roots with no descendant query. The pgid is deliberately NOT
    // stored here: the authoritative pgid lives in `thread_runtime` and
    // attaches/updates after thread creation, so the cascade resolves each
    // descendant's CURRENT pgid at signal time rather than trusting a stale copy.

    /// Record that `parent_thread_id` spawned `child_thread_id`. An exact
    /// re-drive is idempotent; a different parent or relation for an existing
    /// child is rejected as an authority conflict.
    ///
    /// `relation` is a descriptive tag only — the cascade walks every descendant
    /// regardless. Child launches use `"dispatch"`; machine-continuation
    /// successors use `"continuation"`.
    pub fn record_child_link(
        &self,
        parent_thread_id: &str,
        child_thread_id: &str,
        relation: &str,
    ) -> Result<ChildLinkInsertOutcome> {
        self.record_child_link_with_stop_policy(
            parent_thread_id,
            child_thread_id,
            relation,
            ChildLinkStopPolicy::None,
        )
        .map(|(outcome, _)| outcome)
    }

    /// Record a child link and apply its stop policy in one SQLite transaction.
    ///
    /// Conflicting lineage is rejected before any stop tombstone is written.
    /// `IfInserted` therefore means exactly one newly-authorized child, while
    /// `Always` also repairs an exact replay after an interrupted propagation.
    pub fn record_child_link_with_stop_policy(
        &self,
        parent_thread_id: &str,
        child_thread_id: &str,
        relation: &str,
        stop_policy: ChildLinkStopPolicy,
    ) -> Result<(ChildLinkInsertOutcome, Option<StopIntent>)> {
        let tx = self.conn.unchecked_transaction()?;
        let inserted = tx.execute(
            "INSERT INTO thread_child_link (child_thread_id, parent_thread_id, relation, created_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(child_thread_id) DO NOTHING",
            params![
                child_thread_id,
                parent_thread_id,
                relation,
                lillux::time::timestamp_millis()
            ],
        )?;
        let outcome = if inserted == 1 {
            ChildLinkInsertOutcome::Inserted
        } else {
            let existing = tx
                .query_row(
                "SELECT parent_thread_id, relation FROM thread_child_link
                 WHERE child_thread_id = ?1",
                params![child_thread_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "child link insert for {child_thread_id} conflicted but no existing row was found"
                )
            })?;
            if existing.0 != parent_thread_id || existing.1 != relation {
                bail!(
                    "child {child_thread_id} is already linked to parent {} with relation {}; refusing conflicting parent {parent_thread_id} relation {relation}",
                    existing.0,
                    existing.1
                );
            }
            ChildLinkInsertOutcome::AlreadyPresent
        };

        let requested_stop = match stop_policy {
            ChildLinkStopPolicy::None => None,
            ChildLinkStopPolicy::Always(intent) => Some(intent),
            ChildLinkStopPolicy::IfInserted(intent)
                if outcome == ChildLinkInsertOutcome::Inserted =>
            {
                Some(intent)
            }
            ChildLinkStopPolicy::IfInserted(_) => None,
        };
        let effective_stop = if let Some(intent) = requested_stop {
            let updated = tx.execute(
                "UPDATE thread_runtime
                    SET stop_requested_at_ms = COALESCE(stop_requested_at_ms, ?2),
                        stop_intent = CASE
                            WHEN stop_intent = 'kill' OR ?3 = 'kill' THEN 'kill'
                            ELSE 'cancel'
                        END
                  WHERE thread_id = ?1",
                params![
                    child_thread_id,
                    lillux::time::timestamp_millis(),
                    intent.as_str()
                ],
            )?;
            if updated == 0 {
                bail!("thread_runtime row missing for thread_id: {child_thread_id}");
            }
            let persisted: String = tx.query_row(
                "SELECT stop_intent FROM thread_runtime WHERE thread_id = ?1",
                params![child_thread_id],
                |row| row.get(0),
            )?;
            Some(StopIntent::parse(&persisted)?)
        } else {
            None
        };
        tx.commit()?;
        Ok((outcome, effective_stop))
    }

    pub fn child_link_relation(&self, child_thread_id: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT relation FROM thread_child_link WHERE child_thread_id=?1",
                params![child_thread_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Every transitive descendant of `root_thread_id`, breadth-first in spawn
    /// order. `root` itself is excluded, and a `seen` set guards against a link
    /// cycle ever driving an unbounded walk.
    pub fn descendant_thread_ids(&self, root_thread_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT child_thread_id FROM thread_child_link
             WHERE parent_thread_id = ?1
             ORDER BY created_at_ms ASC, child_thread_id ASC",
        )?;

        let mut seen: std::collections::HashSet<String> =
            std::collections::HashSet::from([root_thread_id.to_string()]);
        let mut queue: std::collections::VecDeque<String> =
            std::collections::VecDeque::from([root_thread_id.to_string()]);
        let mut order = Vec::new();

        while let Some(parent) = queue.pop_front() {
            let children = stmt
                .query_map(params![parent], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<String>, _>>()?;
            for child in children {
                if seen.insert(child.clone()) {
                    order.push(child.clone());
                    queue.push_back(child);
                }
            }
        }
        Ok(order)
    }

    // ── Follow waiters ───────────────────────────────────────────────────

    /// Get-or-create a follow reservation by `follow_key` (idempotent). On a
    /// retry the existing row is returned ONLY if the seed agrees — a
    /// conflicting re-drive (same key, different parent/node/step) is rejected
    /// rather than silently reusing a row for a different follow point. New rows
    /// start in phase `reserved`.
    pub fn reserve_follow(&self, seed: &NewFollowWaiter) -> Result<FollowWaiter> {
        if seed.expected_children == 0 {
            bail!(
                "follow reservation {} must expect at least one child",
                seed.follow_key
            );
        }
        let now = lillux::time::timestamp_millis();
        self.conn.execute(
            "INSERT INTO follow_waiter (
                 follow_key, parent_thread_id, parent_chain_root_id,
                 follow_node, graph_run_id, step_count, frontier_id,
                 fanout, expected_children, child_project_authority,
                 phase, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'reserved', ?11, ?11)
             ON CONFLICT(follow_key) DO NOTHING",
            params![
                seed.follow_key,
                seed.parent_thread_id,
                seed.parent_chain_root_id,
                seed.follow_node,
                seed.graph_run_id,
                seed.step_count,
                seed.frontier_id,
                seed.fanout,
                seed.expected_children,
                seed.child_project_authority
                    .as_ref()
                    .map(encode_current_project_authority)
                    .transpose()?,
                now,
            ],
        )?;
        let existing = self.require_follow_waiter(&seed.follow_key)?;
        if existing.parent_thread_id != seed.parent_thread_id
            || existing.parent_chain_root_id != seed.parent_chain_root_id
            || existing.follow_node != seed.follow_node
            || existing.graph_run_id != seed.graph_run_id
            || existing.step_count != seed.step_count
            || existing.frontier_id != seed.frontier_id
            || existing.fanout != seed.fanout
            || existing.expected_children != seed.expected_children
            || (seed.child_project_authority.is_some()
                && existing.child_project_authority != seed.child_project_authority)
        {
            bail!(
                "follow reservation conflict for follow_key {}: seed does not match the persisted row",
                seed.follow_key
            );
        }
        Ok(existing)
    }

    pub fn bind_follow_project_authority(
        &self,
        follow_key: &str,
        authority: &ryeos_state::objects::ExecutionProjectAuthority,
    ) -> Result<()> {
        let encoded = encode_current_project_authority(authority)?;
        self.conn.execute(
            "UPDATE follow_waiter SET child_project_authority=?2, updated_at_ms=?3
             WHERE follow_key=?1 AND child_project_authority IS NULL",
            params![follow_key, encoded, lillux::time::timestamp_millis()],
        )?;
        let persisted = self.require_follow_waiter(follow_key)?;
        if persisted.child_project_authority.as_ref() != Some(authority) {
            bail!("follow waiter {follow_key} was bound to different project authority");
        }
        Ok(())
    }

    /// Record the spawned child's identities. Allowed only when unset (first
    /// write) or already equal (idempotent retry); never overwrites a different
    /// child, which would strand the original.
    // Slot identity, item/spec identity, child lineage, and sealed authority
    // stay explicit because each is independently compared before publication.
    #[allow(clippy::too_many_arguments)]
    pub fn set_follow_child(
        &self,
        follow_key: &str,
        item_index: u32,
        item_ref: &str,
        spec_hash: &str,
        child_thread_id: &str,
        child_chain_root_id: &str,
        sealed_root_request: &crate::thread_lifecycle::SealedRootExecutionRequest,
    ) -> Result<()> {
        if sealed_root_request.item_ref() != item_ref {
            bail!("follow child sealed authority does not match slot item_ref");
        }
        let tx = self.conn.unchecked_transaction()?;
        let sealed_root_request = lillux::canonical_json(
            &serde_json::to_value(sealed_root_request)
                .context("encode sealed follow-child root request")?,
        )
        .context("canonicalize sealed follow-child root request")?;
        let expected_children = tx
            .query_row(
                "SELECT expected_children FROM follow_waiter WHERE follow_key = ?1",
                params![follow_key],
                |r| r.get::<_, u32>(0),
            )
            .optional()?
            .ok_or_else(|| {
                anyhow::anyhow!("follow waiter row missing for follow_key: {follow_key}")
            })?;
        if item_index >= expected_children {
            bail!("follow waiter {follow_key} child index {item_index} is out of range");
        }
        let now = lillux::time::timestamp_millis();
        tx.execute("INSERT INTO follow_waiter_child
            (follow_key,item_index,item_ref,spec_hash,child_thread_id,child_chain_root_id,sealed_root_request,created_at_ms,updated_at_ms)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8) ON CONFLICT(follow_key,item_index) DO NOTHING",
            params![follow_key,item_index,item_ref,spec_hash,child_thread_id,child_chain_root_id,sealed_root_request,now])?;
        let child = tx
            .query_row(
                "SELECT item_ref,spec_hash,child_thread_id,child_chain_root_id,sealed_root_request
            FROM follow_waiter_child WHERE follow_key=?1 AND item_index=?2",
                params![follow_key, item_index],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "follow waiter {follow_key} child index {item_index} was not persisted"
                )
            })?;
        if child.0 != item_ref
            || child.1 != spec_hash
            || child.2 != child_thread_id
            || child.3 != child_chain_root_id
            || child.4 != sealed_root_request
        {
            bail!(
                "follow waiter {follow_key} child index {item_index} conflicts with persisted child/spec"
            );
        }
        tx.commit()?;
        Ok(())
    }

    /// Record the parent's (un-launched) follow-resume successor. Allowed only
    /// when unset or already equal; never overwrites a different successor.
    pub fn set_follow_parent_successor(
        &self,
        follow_key: &str,
        successor_thread_id: &str,
    ) -> Result<()> {
        let w = self.require_follow_waiter(follow_key)?;
        match w.parent_successor_thread_id.as_deref() {
            None => {}
            Some(s) if s == successor_thread_id => return Ok(()),
            _ => bail!(
                "follow waiter {follow_key} already has a different parent successor; refusing to overwrite"
            ),
        }
        self.conn.execute(
            "UPDATE follow_waiter
                SET parent_successor_thread_id = ?2, updated_at_ms = ?3
              WHERE follow_key = ?1",
            params![
                follow_key,
                successor_thread_id,
                lillux::time::timestamp_millis()
            ],
        )?;
        Ok(())
    }

    /// Transition a reserved waiter to its durable post-suspension phase. A
    /// complete cohort advances directly to `ready`; otherwise it advances to
    /// `waiting`. Idempotent only on `waiting` and never regresses a later phase.
    pub fn mark_follow_waiting(&self, follow_key: &str) -> Result<String> {
        let tx = self.conn.unchecked_transaction()?;
        let w = self.require_follow_waiter(follow_key)?;
        if w.phase == follow_phase::WAITING {
            tx.commit()?;
            return Ok(follow_phase::WAITING.to_string());
        }
        if w.phase != follow_phase::RESERVED {
            bail!(
                "follow waiter {follow_key} cannot transition {} -> waiting",
                w.phase
            );
        }
        if w.parent_successor_thread_id.is_none()
            || w.children.len() != w.expected_children as usize
            || w.children
                .iter()
                .enumerate()
                .any(|(i, c)| c.item_index as usize != i)
        {
            bail!(
                "follow waiter {follow_key} cannot mark waiting before child + successor are recorded"
            );
        }
        let complete = validate_terminal_completeness(&w)?;
        let target = if complete {
            follow_phase::READY
        } else {
            follow_phase::WAITING
        };
        let changed = tx.execute(
            "UPDATE follow_waiter SET phase=?2, updated_at_ms=?3
            WHERE follow_key=?1 AND phase='reserved'",
            params![follow_key, target, lillux::time::timestamp_millis()],
        )?;
        if changed != 1 {
            bail!("follow waiter {follow_key} reserved transition raced");
        }
        tx.commit()?;
        Ok(target.to_string())
    }

    /// Transition → resuming. Only `ready → resuming` (idempotent on
    /// `resuming`); requires the terminal envelope + successor present.
    pub fn mark_follow_resuming(&self, follow_key: &str) -> Result<()> {
        let w = self.require_follow_waiter(follow_key)?;
        if w.phase == follow_phase::RESUMING {
            return Ok(());
        }
        if w.phase != follow_phase::READY {
            bail!(
                "follow waiter {follow_key} cannot transition {} -> resuming",
                w.phase
            );
        }
        if w.parent_successor_thread_id.is_none() || !validate_terminal_completeness(&w)? {
            bail!("follow waiter {follow_key} cannot resume without terminal envelope + successor");
        }
        let changed = self.conn.execute(
            "UPDATE follow_waiter SET phase='resuming', updated_at_ms=?2
            WHERE follow_key=?1 AND phase='ready'",
            params![follow_key, lillux::time::timestamp_millis()],
        )?;
        if changed != 1 {
            bail!("follow waiter {follow_key} ready transition raced");
        }
        Ok(())
    }

    fn require_follow_waiter(&self, follow_key: &str) -> Result<FollowWaiter> {
        self.get_follow_waiter_by_key(follow_key)?.ok_or_else(|| {
            anyhow::anyhow!("follow waiter row missing for follow_key: {follow_key}")
        })
    }

    /// Mark the followed child chain terminal, keyed by the child's chain root.
    /// Stores the canonical terminal envelope and flips the waiter to `ready`.
    ///
    /// Idempotent and immutable once captured. Terminal data is recorded even
    /// while the waiter is `reserved`, closing the callback-before-waiting race.
    /// Only `waiting` may transition to `ready`; `ready` and `resuming` are never
    /// regressed. Returns `true` only on the first `waiting → ready` transition.
    pub fn mark_follow_child_terminal(
        &self,
        child_chain_root_id: &str,
        child_terminal_thread_id: &str,
        child_terminal_status: &str,
        terminal_envelope: &Value,
    ) -> Result<bool> {
        let envelope_json = serde_json::to_string(terminal_envelope)
            .context("failed to encode follow terminal envelope")?;
        let tx = self.conn.unchecked_transaction()?;
        let child = tx
            .query_row(
                "SELECT c.follow_key, c.item_index, w.phase,
                        c.terminal_thread_id, c.terminal_status, c.terminal_envelope
                   FROM follow_waiter_child c
                   JOIN follow_waiter w ON w.follow_key = c.follow_key
                  WHERE c.child_chain_root_id = ?1",
                params![child_chain_root_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, u32>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            follow_key,
            item_index,
            _phase,
            terminal_thread_id,
            terminal_status,
            stored_envelope,
        )) = child
        else {
            tx.commit()?;
            return Ok(false);
        };

        if terminal_thread_id.is_some() || terminal_status.is_some() || stored_envelope.is_some() {
            if terminal_thread_id.is_none()
                || terminal_status.is_none()
                || stored_envelope.is_none()
            {
                bail!(
                    "follow child chain {child_chain_root_id} has a partial persisted terminal tuple"
                );
            }
            let same_envelope = stored_envelope
                .as_deref()
                .map(serde_json::from_str::<Value>)
                .transpose()
                .context("failed to decode persisted follow terminal envelope")?
                .as_ref()
                == Some(terminal_envelope);
            if terminal_thread_id.as_deref() == Some(child_terminal_thread_id)
                && terminal_status.as_deref() == Some(child_terminal_status)
                && same_envelope
            {
                tx.commit()?;
                return Ok(false);
            }
            bail!(
                "follow child chain {child_chain_root_id} already has a different terminal result"
            );
        }

        let now = lillux::time::timestamp_millis();
        tx.execute(
            "UPDATE follow_waiter_child
                SET terminal_thread_id = ?3,
                    terminal_status = ?4,
                    terminal_envelope = ?5,
                    updated_at_ms = ?6
              WHERE follow_key = ?1 AND item_index = ?2",
            params![
                follow_key,
                item_index,
                child_terminal_thread_id,
                child_terminal_status,
                envelope_json,
                now
            ],
        )?;
        let flipped = tx.execute(
            "UPDATE follow_waiter
                SET phase = 'ready', updated_at_ms = ?2
              WHERE follow_key = ?1
                AND phase = 'waiting'
                AND (SELECT COUNT(*) FROM follow_waiter_child
                      WHERE follow_key = ?1 AND terminal_thread_id IS NOT NULL
                        AND terminal_status IS NOT NULL AND terminal_envelope IS NOT NULL) = expected_children",
            params![follow_key, now],
        )? == 1;
        tx.commit()?;
        Ok(flipped)
    }

    pub fn get_follow_waiter_by_key(&self, follow_key: &str) -> Result<Option<FollowWaiter>> {
        let waiter = self
            .conn
            .query_row(
                &format!("SELECT {FOLLOW_WAITER_COLUMNS} FROM follow_waiter WHERE follow_key = ?1"),
                params![follow_key],
                read_follow_waiter_row,
            )
            .optional()?;
        waiter.map(|w| self.with_follow_children(w)).transpose()
    }

    pub fn get_follow_waiter_by_child_chain(
        &self,
        child_chain_root_id: &str,
    ) -> Result<Option<FollowWaiter>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {FOLLOW_WAITER_COLUMNS} FROM follow_waiter WHERE follow_key =
                     (SELECT follow_key FROM follow_waiter_child WHERE child_chain_root_id = ?1)"
                ),
                params![child_chain_root_id],
                read_follow_waiter_row,
            )
            .optional()?
            .map(|w| self.with_follow_children(w))
            .transpose()
    }

    /// The follow waiter for which `parent_thread_id` is the SUSPENDED PARENT —
    /// the thread that issued the follow and settled `continued` awaiting its
    /// child chain. A suspended parent carries at most one live waiter (the
    /// parent re-drives the same `follow_key` idempotently, and it cannot issue
    /// another follow until resumed as a fresh successor thread), so this reads a
    /// single row. Used to decorate a `continued` thread with its follow lineage.
    pub fn get_follow_waiter_by_parent_thread(
        &self,
        parent_thread_id: &str,
    ) -> Result<Option<FollowWaiter>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {FOLLOW_WAITER_COLUMNS} FROM follow_waiter \
                     WHERE parent_thread_id = ?1 ORDER BY created_at_ms DESC LIMIT 1"
                ),
                params![parent_thread_id],
                read_follow_waiter_row,
            )
            .optional()?
            .map(|w| self.with_follow_children(w))
            .transpose()
    }

    /// The follow waiter whose recorded resume successor is `successor_thread_id`
    /// (the `parent_successor_thread_id` UNIQUE index). Used to decorate a
    /// follow-resume successor with its live lineage while the waiter exists;
    /// once the waiter is cleared the successor is recognized instead from the
    /// projected `graph_follow_resume` continuation edge (CAS is truth).
    pub fn get_follow_waiter_by_successor(
        &self,
        successor_thread_id: &str,
    ) -> Result<Option<FollowWaiter>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {FOLLOW_WAITER_COLUMNS} FROM follow_waiter \
                     WHERE parent_successor_thread_id = ?1"
                ),
                params![successor_thread_id],
                read_follow_waiter_row,
            )
            .optional()?
            .map(|w| self.with_follow_children(w))
            .transpose()
    }

    /// Response-facing follow facts for a bounded set of thread ids. A thread
    /// can match either side of the waiter (suspended parent or resume
    /// successor). The query is chunked below SQLite's parameter ceiling and
    /// deliberately projects no child terminal envelope.
    pub fn follow_waiter_summaries_for_threads(
        &self,
        thread_ids: &[String],
        max_items: usize,
    ) -> Result<Vec<FollowWaiterSummary>> {
        if max_items == 0 {
            bail!("follow waiter summary maximum must be positive");
        }
        if thread_ids.len() > max_items {
            bail!(
                "follow waiter summary requested {} threads; maximum is {max_items}",
                thread_ids.len()
            );
        }
        if thread_ids.is_empty() {
            return Ok(Vec::new());
        }
        let query_limit = max_items
            .checked_add(1)
            .context("follow waiter summary limit overflow")?;
        let query_limit =
            i64::try_from(query_limit).context("follow waiter summary limit exceeds SQLite i64")?;
        let mut summaries = std::collections::BTreeMap::new();
        for batch in thread_ids.chunks(FOLLOW_WAITER_SUMMARY_QUERY_BATCH) {
            let requested_rows = std::iter::repeat_n("(?)", batch.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "WITH requested(thread_id) AS (VALUES {requested_rows}) \
                 SELECT {FOLLOW_WAITER_SUMMARY_COLUMNS} FROM follow_waiter fw \
                 WHERE fw.parent_thread_id IN (SELECT thread_id FROM requested) \
                    OR fw.parent_successor_thread_id IN (SELECT thread_id FROM requested) \
                 ORDER BY fw.created_at_ms, fw.follow_key LIMIT ?"
            );
            let mut params: Vec<&dyn rusqlite::types::ToSql> = batch
                .iter()
                .map(|thread_id| thread_id as &dyn rusqlite::types::ToSql)
                .collect();
            params.push(&query_limit);
            let mut stmt = self
                .conn
                .prepare(&sql)
                .context("prepare scoped follow waiter summaries")?;
            let rows = stmt
                .query_map(params.as_slice(), read_follow_waiter_summary_row)
                .context("query scoped follow waiter summaries")?;
            for row in rows {
                let summary = row.context("read scoped follow waiter summary")?;
                summaries.insert(summary.follow_key.clone(), summary);
                if summaries.len() > max_items {
                    bail!("thread list has more than {max_items} matching follow waiters");
                }
            }
        }
        let mut summaries = summaries.into_values().collect::<Vec<_>>();
        summaries.sort_by(|a, b| {
            a.created_at_ms
                .cmp(&b.created_at_ms)
                .then_with(|| a.follow_key.cmp(&b.follow_key))
        });
        Ok(summaries)
    }

    /// A complete but fail-closed snapshot for active/project list discovery.
    /// Reading one extra row distinguishes a complete result from truncation;
    /// callers never receive an incomplete set of suspended parents.
    pub fn follow_waiter_summaries_bounded(
        &self,
        max_items: usize,
    ) -> Result<Vec<FollowWaiterSummary>> {
        if max_items == 0 {
            bail!("follow waiter summary maximum must be positive");
        }
        let query_limit = max_items
            .checked_add(1)
            .context("follow waiter summary limit overflow")?;
        let query_limit =
            i64::try_from(query_limit).context("follow waiter summary limit exceeds SQLite i64")?;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {FOLLOW_WAITER_SUMMARY_COLUMNS} FROM follow_waiter fw \
             ORDER BY fw.created_at_ms, fw.follow_key LIMIT ?1"
        ))?;
        let rows = stmt.query_map(params![query_limit], read_follow_waiter_summary_row)?;
        let summaries = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        if summaries.len() > max_items {
            bail!("thread list has more than {max_items} live follow waiters");
        }
        Ok(summaries)
    }

    /// All active follow waiters. The table holds only non-cleared rows, so
    /// every row here is recoverable by reconcile.
    pub fn list_follow_waiters(&self) -> Result<Vec<FollowWaiter>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {FOLLOW_WAITER_COLUMNS} FROM follow_waiter ORDER BY created_at_ms ASC"
        ))?;
        let rows = stmt.query_map([], read_follow_waiter_row)?;
        let mut waiters = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut child_stmt = self.conn.prepare(
            "SELECT item_index,item_ref,spec_hash,child_thread_id,child_chain_root_id,
             sealed_root_request,terminal_thread_id,terminal_status,terminal_envelope,created_at_ms,updated_at_ms,
             follow_key
             FROM follow_waiter_child ORDER BY follow_key,item_index",
        )?;
        let child_rows = child_stmt.query_map([], |row| {
            Ok((row.get::<_, String>(11)?, read_follow_child_row(row)?))
        })?;
        let mut children_by_waiter = std::collections::HashMap::new();
        for row in child_rows {
            let (follow_key, child) = row?;
            children_by_waiter
                .entry(follow_key)
                .or_insert_with(Vec::new)
                .push(child);
        }
        for waiter in &mut waiters {
            waiter.children = children_by_waiter
                .remove(&waiter.follow_key)
                .unwrap_or_default();
        }
        Ok(waiters)
    }

    pub fn get_follow_child(
        &self,
        follow_key: &str,
        item_index: u32,
    ) -> Result<Option<FollowWaiterChild>> {
        self.conn
            .query_row(
                "SELECT item_index,item_ref,spec_hash,child_thread_id,child_chain_root_id,
            sealed_root_request,terminal_thread_id,terminal_status,terminal_envelope,created_at_ms,updated_at_ms
            FROM follow_waiter_child WHERE follow_key=?1 AND item_index=?2",
                params![follow_key, item_index],
                read_follow_child_row,
            )
            .optional()
            .map_err(Into::into)
    }

    fn with_follow_children(&self, mut waiter: FollowWaiter) -> Result<FollowWaiter> {
        let mut stmt = self.conn.prepare(
            "SELECT item_index,item_ref,spec_hash,child_thread_id,child_chain_root_id,
            sealed_root_request,terminal_thread_id,terminal_status,terminal_envelope,created_at_ms,updated_at_ms
            FROM follow_waiter_child WHERE follow_key=?1 ORDER BY item_index",
        )?;
        waiter.children = stmt
            .query_map(params![waiter.follow_key], read_follow_child_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(waiter)
    }

    /// Delete a follow waiter — only once the parent successor is independently
    /// recoverable (checkpoint copied with the result + launch claimed, or the
    /// successor reached terminal).
    pub fn clear_follow_waiter(&self, follow_key: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM follow_waiter_child WHERE follow_key=?1",
            params![follow_key],
        )?;
        tx.execute(
            "DELETE FROM follow_waiter WHERE follow_key = ?1",
            params![follow_key],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Re-arm the auto-resume budget. A graceful daemon shutdown kills a
    /// thread's process deliberately — that death is the operator's, not the
    /// thread's, so it must not consume `max_auto_resume_attempts`. Daemon
    /// CRASHES never run the drain, so a crash loop still exhausts the
    /// budget.
    pub fn reset_resume_attempts(&self, thread_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE thread_runtime SET resume_attempts = 0 WHERE thread_id = ?1",
            params![thread_id],
        )?;
        Ok(())
    }

    // ── Launch windows (bounded detached fanout) ────────────────────────
    //
    // A window member is a detached child CHAIN: the row is keyed by the
    // child's chain_root_id so a slot survives `thread_continued`
    // transitions (a suspending agent stays one live member) and is
    // released only when the chain reaches a hard terminal. Rows with
    // `launched_at_ms` NULL are queued; the row is deleted at release, so
    // live-slot count == launched rows present. All access is serialized
    // by the state-store lock; a crash between insert and admit leaves a
    // queued row the sweep admits later.

    pub fn launch_window_insert(
        &self,
        child_chain_root_id: &str,
        window_key: &str,
        width: u32,
        now_ms: i64,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "INSERT OR IGNORE INTO launch_window
                 (child_chain_root_id, window_key, width, created_at_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![child_chain_root_id, window_key, width, now_ms],
        )? != 0)
    }

    fn launch_window_live_count(&self, window_key: &str) -> Result<u32> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM launch_window
             WHERE window_key = ?1 AND launched_at_ms IS NOT NULL",
            params![window_key],
            |r| r.get(0),
        )?)
    }

    /// Node-resource occupancy, distinct from each window's logical width.
    /// A launched chain that is durably suspended in `follow` keeps its
    /// originating window slot, but it has no running runtime and must yield
    /// its node slot to the followed child. Excluding exactly `waiting`
    /// parents prevents a finite global ceiling from deadlocking nested
    /// fanout while `ready`/`resuming` parents count again before execution.
    fn launch_window_live_total(&self) -> Result<u32> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*)
               FROM launch_window AS lw
              WHERE lw.launched_at_ms IS NOT NULL
                AND NOT EXISTS (
                    SELECT 1
                      FROM follow_waiter AS fw
                     WHERE fw.parent_chain_root_id = lw.child_chain_root_id
                       AND fw.phase = 'waiting'
                )",
            [],
            |r| r.get(0),
        )?)
    }

    /// Admit queued members of one window, oldest first, up to the window
    /// width and the optional daemon-global live ceiling. Marks admitted
    /// rows launched and returns their chain roots — the caller owns
    /// actually launching them.
    #[cfg(test)]
    fn launch_window_admit(
        &self,
        window_key: &str,
        global_live_limit: Option<u32>,
        now_ms: i64,
    ) -> Result<Vec<String>> {
        // Follow cohorts stage their complete membership before the parent is
        // irreversibly continued. Keep those rows ineligible until the waiter
        // durably reaches `waiting`; this gate is inside the primitive so direct
        // admission, terminal-release admission, and maintenance sweeps all obey
        // the same ordering. `ready`/`resuming` are not launchable phases: they
        // prove the complete child cohort is already terminal. A missing waiter
        // also fails closed. Detached windows do not use the `follow:` namespace.
        if let Some(follow_key) = window_key.strip_prefix("follow:") {
            let phase = self
                .conn
                .query_row(
                    "SELECT phase FROM follow_waiter WHERE follow_key = ?1",
                    params![follow_key],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if phase.as_deref() != Some(follow_phase::WAITING) {
                return Ok(Vec::new());
            }
        }

        let mut admitted = Vec::new();
        loop {
            let candidate: Option<(String, u32)> = self
                .conn
                .query_row(
                    "SELECT child_chain_root_id, width FROM launch_window
                     WHERE window_key = ?1 AND launched_at_ms IS NULL AND cancelled_at_ms IS NULL
                     ORDER BY rowid ASC LIMIT 1",
                    params![window_key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((chain_root, width)) = candidate else {
                break;
            };
            if self.launch_window_live_count(window_key)? >= width {
                break;
            }
            if let Some(cap) = global_live_limit
                && self.launch_window_live_total()? >= cap
            {
                break;
            }
            self.conn.execute(
                "UPDATE launch_window SET launched_at_ms = ?2 WHERE child_chain_root_id = ?1",
                params![chain_root, now_ms],
            )?;
            admitted.push(chain_root);
        }
        Ok(admitted)
    }

    fn launch_window_phase_is_eligible(&self, window_key: &str) -> Result<bool> {
        let Some(follow_key) = window_key.strip_prefix("follow:") else {
            return Ok(true);
        };
        let phase = self
            .conn
            .query_row(
                "SELECT phase FROM follow_waiter WHERE follow_key = ?1",
                params![follow_key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(phase.as_deref() == Some(follow_phase::WAITING))
    }

    /// Admit the oldest eligible members across every launch window. This is
    /// the node-wide fairness boundary: freeing a slot in one cohort must wake
    /// an older queued member in another cohort immediately, not at the next
    /// maintenance sweep.
    pub fn launch_window_admit_global(
        &self,
        global_live_limit: Option<u32>,
        now_ms: i64,
    ) -> Result<Vec<String>> {
        let mut admitted = Vec::new();
        loop {
            if let Some(cap) = global_live_limit
                && self.launch_window_live_total()? >= cap
            {
                break;
            }
            let mut statement = self.conn.prepare(
                "SELECT child_chain_root_id, window_key, width
                   FROM launch_window
                  WHERE launched_at_ms IS NULL AND cancelled_at_ms IS NULL
                  ORDER BY created_at_ms ASC, rowid ASC",
            )?;
            let candidates = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, u32>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut selected = None;
            for (chain_root, window_key, width) in candidates {
                if self.launch_window_phase_is_eligible(&window_key)?
                    && self.launch_window_live_count(&window_key)? < width
                {
                    selected = Some(chain_root);
                    break;
                }
            }
            let Some(chain_root) = selected else {
                break;
            };
            self.conn.execute(
                "UPDATE launch_window SET launched_at_ms = ?2
                  WHERE child_chain_root_id = ?1
                    AND launched_at_ms IS NULL AND cancelled_at_ms IS NULL",
                params![chain_root, now_ms],
            )?;
            admitted.push(chain_root);
        }
        Ok(admitted)
    }

    /// Release a finished window member (its chain reached a hard terminal)
    /// and admit the window's next queued members. Empty for a chain that
    /// holds no window row.
    pub fn launch_window_release(
        &self,
        child_chain_root_id: &str,
        global_live_limit: Option<u32>,
        now_ms: i64,
    ) -> Result<Vec<String>> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM launch_window WHERE child_chain_root_id = ?1)",
            params![child_chain_root_id],
            |r| r.get(0),
        )?;
        if !exists {
            return Ok(Vec::new());
        }
        self.conn.execute(
            "DELETE FROM launch_window WHERE child_chain_root_id = ?1",
            params![child_chain_root_id],
        )?;
        self.launch_window_admit_global(global_live_limit, now_ms)
    }

    /// Remove exactly the requested members that are still queued. This is used
    /// by cancellation and intentionally does not admit replacements.
    pub fn launch_window_cancel_queued(
        &mut self,
        chain_roots: &[String],
        now_ms: i64,
    ) -> Result<Vec<String>> {
        let tx = self.conn.transaction()?;
        let mut removed = Vec::new();
        for root in chain_roots {
            if tx.execute(
                "UPDATE launch_window SET cancelled_at_ms = ?2
                 WHERE child_chain_root_id = ?1 AND launched_at_ms IS NULL AND cancelled_at_ms IS NULL",
                params![root, now_ms],
            )? != 0
            {
                removed.push(root.clone());
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Tombstone selected members regardless of admission marker. Callers must
    /// first prove from the authoritative thread row that no process is live.
    pub fn launch_window_cancel_members(
        &mut self,
        chain_roots: &[String],
        now_ms: i64,
    ) -> Result<Vec<String>> {
        let tx = self.conn.transaction()?;
        let mut cancelled = Vec::new();
        for root in chain_roots {
            if tx.execute(
                "UPDATE launch_window SET cancelled_at_ms = ?2
                 WHERE child_chain_root_id = ?1 AND cancelled_at_ms IS NULL",
                params![root, now_ms],
            )? != 0
            {
                cancelled.push(root.clone());
            }
        }
        tx.commit()?;
        Ok(cancelled)
    }

    pub fn launch_window_is_cancelled(&self, child_chain_root_id: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT 1 FROM launch_window WHERE child_chain_root_id = ?1 AND cancelled_at_ms IS NOT NULL",
            params![child_chain_root_id], |_| Ok(()),
        ).optional()?.is_some())
    }

    pub fn launch_window_cancelled_members(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT child_chain_root_id FROM launch_window WHERE cancelled_at_ms IS NOT NULL ORDER BY rowid")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn launch_window_discard_member(&self, chain_root: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM launch_window WHERE child_chain_root_id = ?1",
            params![chain_root],
        )?;
        Ok(())
    }

    /// Whether this chain is a window member deliberately awaiting admission
    /// — reconcile must leave such a `created` row alone rather than
    /// finalize it as an interrupted spawn.
    pub fn launch_window_is_queued(&self, child_chain_root_id: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM launch_window
                 WHERE child_chain_root_id = ?1 AND launched_at_ms IS NULL",
                params![child_chain_root_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Whether this chain holds ANY window row (queued or launched) — the
    /// cheap pre-check every finalize seam runs before chain-walking.
    pub fn launch_window_is_member(&self, child_chain_root_id: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM launch_window WHERE child_chain_root_id = ?1",
                params![child_chain_root_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Every slot-holding (launched, unreleased) member — drift-repair input
    /// for the sweep, which releases any whose chain died without a kick.
    pub fn launch_window_launched_members(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT child_chain_root_id FROM launch_window
             WHERE launched_at_ms IS NOT NULL ORDER BY rowid ASC",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every window key with queued members — sweep admission input.
    pub fn launch_window_keys_with_queue(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT window_key FROM launch_window WHERE launched_at_ms IS NULL AND cancelled_at_ms IS NULL",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

const FOLLOW_WAITER_COLUMNS: &str = "follow_key, parent_thread_id, parent_chain_root_id, \
     parent_successor_thread_id, follow_node, graph_run_id, step_count, frontier_id, \
     fanout, expected_children, child_project_authority, phase, created_at_ms, updated_at_ms";

const FOLLOW_WAITER_SUMMARY_QUERY_BATCH: usize = 500;
const FOLLOW_WAITER_SUMMARY_COLUMNS: &str = "fw.follow_key, fw.parent_thread_id, \
     fw.parent_successor_thread_id, fw.follow_node, fw.phase, fw.fanout, \
     fw.expected_children, \
     (SELECT c.child_thread_id FROM follow_waiter_child c \
       WHERE c.follow_key = fw.follow_key ORDER BY c.item_index LIMIT 1), \
     (SELECT c.child_chain_root_id FROM follow_waiter_child c \
       WHERE c.follow_key = fw.follow_key ORDER BY c.item_index LIMIT 1), \
     (SELECT c.terminal_status FROM follow_waiter_child c \
       WHERE c.follow_key = fw.follow_key ORDER BY c.item_index LIMIT 1), \
     (SELECT COUNT(*) FROM follow_waiter_child c WHERE c.follow_key = fw.follow_key), \
     (SELECT COUNT(*) FROM follow_waiter_child c \
       WHERE c.follow_key = fw.follow_key AND c.terminal_status IS NOT NULL), \
     fw.created_at_ms";

fn read_follow_waiter_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FollowWaiter> {
    let child_project_authority = row
        .get::<_, Option<String>>(10)?
        .map(|raw| decode_current_project_authority_column(10, &raw))
        .transpose()?;
    Ok(FollowWaiter {
        follow_key: row.get(0)?,
        parent_thread_id: row.get(1)?,
        parent_chain_root_id: row.get(2)?,
        parent_successor_thread_id: row.get(3)?,
        follow_node: row.get(4)?,
        graph_run_id: row.get(5)?,
        step_count: row.get(6)?,
        frontier_id: row.get(7)?,
        fanout: row.get(8)?,
        expected_children: row.get(9)?,
        child_project_authority,
        children: Vec::new(),
        phase: row.get(11)?,
        created_at_ms: row.get(12)?,
        updated_at_ms: row.get(13)?,
    })
}

fn read_follow_waiter_summary_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<FollowWaiterSummary> {
    Ok(FollowWaiterSummary {
        follow_key: row.get(0)?,
        parent_thread_id: row.get(1)?,
        parent_successor_thread_id: row.get(2)?,
        follow_node: row.get(3)?,
        phase: row.get(4)?,
        fanout: row.get(5)?,
        expected_children: row.get(6)?,
        first_child_thread_id: row.get(7)?,
        first_child_chain_root_id: row.get(8)?,
        first_child_terminal_status: row.get(9)?,
        child_count: row.get(10)?,
        terminal_child_count: row.get(11)?,
        created_at_ms: row.get(12)?,
    })
}

fn read_follow_child_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FollowWaiterChild> {
    let sealed_raw: String = row.get(5)?;
    let sealed_root_request = serde_json::from_str(&sealed_raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let raw: Option<String> = row.get(8)?;
    let terminal_envelope = raw
        .map(|s| {
            serde_json::from_str(&s).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    8,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })
        })
        .transpose()?;
    Ok(FollowWaiterChild {
        item_index: row.get(0)?,
        item_ref: row.get(1)?,
        spec_hash: row.get(2)?,
        child_thread_id: row.get(3)?,
        child_chain_root_id: row.get(4)?,
        sealed_root_request,
        terminal_thread_id: row.get(6)?,
        terminal_status: row.get(7)?,
        terminal_envelope,
        created_at_ms: row.get(9)?,
        updated_at_ms: row.get(10)?,
    })
}

fn validate_terminal_completeness(waiter: &FollowWaiter) -> Result<bool> {
    let mut complete = 0usize;
    for child in &waiter.children {
        match (
            child.terminal_thread_id.is_some(),
            child.terminal_status.is_some(),
            child.terminal_envelope.is_some(),
        ) {
            (false, false, false) => {}
            (true, true, true) => complete += 1,
            _ => bail!(
                "follow waiter {} child index {} has a partial terminal tuple",
                waiter.follow_key,
                child.item_index
            ),
        }
    }
    Ok(waiter.children.len() == waiter.expected_children as usize
        && complete == waiter.expected_children as usize)
}

fn read_command_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CommandRecord> {
    Ok(CommandRecord {
        command_id: row.get(0)?,
        thread_id: row.get(1)?,
        command_type: row.get(2)?,
        status: row.get(3)?,
        requested_by: row.get(4)?,
        params: parse_json_blob(row.get(5)?)?,
        result: parse_json_blob(row.get(6)?)?,
        created_at: row.get(7)?,
        claimed_at: row.get(8)?,
        completed_at: row.get(9)?,
    })
}

fn read_bounded_command_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CommandRecord> {
    for (index, maximum, label) in [
        (10, MAX_COMMAND_REQUESTED_BY_BYTES, "command requested_by"),
        (11, MAX_COMMAND_PARAMS_BYTES, "command params"),
        (12, MAX_COMMAND_RESULT_BYTES, "command result"),
    ] {
        let Some(length) = row.get::<_, Option<i64>>(index)? else {
            continue;
        };
        let length = usize::try_from(length).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?;
        if length > maximum {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Integer,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{label} is {length} bytes; maximum is {maximum}"),
                )
                .into(),
            ));
        }
    }
    read_command_row(row)
}

fn now_rfc3339() -> String {
    lillux::time::iso8601_now()
}

/// Whether a thread's terminal status fulfils a control command's intent — a
/// `cancel` that ended `cancelled`, or a `kill` that ended `killed`. Used to
/// settle such a command `completed` (it took effect) rather than `rejected`.
fn command_fulfilled_by_terminal(command_type: &str, terminal_status: &str) -> bool {
    matches!(
        (command_type, terminal_status),
        ("cancel", "cancelled") | ("kill", "killed")
    )
}

fn json_blob(value: &Option<Value>) -> Result<Option<Vec<u8>>> {
    value
        .as_ref()
        .map(serde_json::to_vec)
        .transpose()
        .context("failed to encode json blob")
}

fn json_blob_ref(value: Option<&Value>) -> Result<Option<Vec<u8>>> {
    value
        .map(serde_json::to_vec)
        .transpose()
        .context("failed to encode json blob")
}

fn parse_json_blob(blob: Option<Vec<u8>>) -> rusqlite::Result<Option<Value>> {
    blob.map(|bytes| serde_json::from_slice(&bytes))
        .transpose()
        .map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, Box::new(err))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch_metadata::RuntimeLaunchMetadata;
    use ryeos_engine::contracts::CancellationMode;
    use tempfile::TempDir;

    fn fresh_db() -> (TempDir, RuntimeDb) {
        let tmp = TempDir::new().unwrap();
        let db = RuntimeDb::open(&tmp.path().join("runtime.db")).unwrap();
        (tmp, db)
    }

    fn in_process_launch_metadata() -> RuntimeLaunchMetadata {
        RuntimeLaunchMetadata::default()
            .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
            .with_in_process_lifecycle_authority(
                ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_NON_RECOVERABLE,
            )
    }

    fn create_locked_profile(db: &RuntimeDb, profile_id: &str, lock_owner: &str) {
        db.create_credential_profile(NewCredentialProfile {
            profile_id,
            owner_principal: "fp:operator",
            home_id: &format!("home-{profile_id}"),
        })
        .unwrap();
        db.acquire_credential_profile(profile_id, "fp:operator", lock_owner)
            .unwrap();
    }

    #[test]
    fn dedicated_worker_attachment_atomically_owns_process_workspace_and_session() {
        let (_tmp, db) = fresh_db();
        create_locked_profile(&db, "P-one", "worker-one");
        db.conn
            .execute(
                "INSERT INTO execution_workspace (
                    workspace_id, thread_id, launch_owner, backend_id,
                    lower_snapshot, root_path, state, created_at_ms, updated_at_ms
                 ) VALUES ('W-one', 'T-root', 'dedicated_worker_session', 'trusted-daemon',
                           'a', '/tmp/workspace-one', 'ready', 1, 1)",
                [],
            )
            .unwrap();
        db.admit_dedicated_session(NewDedicatedSession {
            session_id: "S-one",
            root_thread_id: "T-root",
            owner_principal: "fp:operator",
            admitted_capsule_hash: &"a".repeat(64),
            workspace_id: "W-one",
            candidate_required: false,
            credential_profile_id: "P-one",
            credential_generation: 1,
            credential_lock_owner: "worker-one",
        })
        .unwrap();
        let record = WorkerProcessRecord {
            worker_instance_id: "worker-one".to_owned(),
            boot_identity_hash: "b".repeat(64),
            session_capsule_hash: "a".repeat(64),
            boot_epoch: 1,
            lifecycle_generation: 1,
            process_identity: fake_process_identity(123, 123),
            control_channel_identity: "fd:9".to_owned(),
            state: WorkerProcessState::Attached,
            daemon_generation_id: "daemon-one".to_owned(),
            session_id: "S-one".to_owned(),
            cleanup_state: "owned".to_owned(),
            created_at_ms: 2,
            updated_at_ms: 2,
        };
        db.attach_worker_process(&record).unwrap();
        assert_eq!(db.worker_process("worker-one").unwrap(), Some(record));
        let workspace_state: String = db
            .conn
            .query_row(
                "SELECT state FROM execution_workspace WHERE workspace_id = 'W-one'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let session_state: String = db
            .conn
            .query_row(
                "SELECT state FROM dedicated_session WHERE session_id = 'S-one'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(workspace_state, "active");
        assert_eq!(session_state, "binding");
        db.complete_worker_binding("worker-one", "S-one", 1)
            .unwrap();
        assert_eq!(
            db.worker_process("worker-one").unwrap().unwrap().state,
            WorkerProcessState::Live
        );
        let session_state: String = db
            .conn
            .query_row(
                "SELECT state FROM dedicated_session WHERE session_id = 'S-one'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(session_state, "idle");
        db.settle_worker_process("worker-one", "S-one", 1, "reaped", "fixture restart")
            .unwrap();
        let settled = db.worker_process("worker-one").unwrap().unwrap();
        assert_eq!(settled.state, WorkerProcessState::Dead);
        assert_eq!(settled.cleanup_state, "reaped");
        db.terminalize_dedicated_session("S-one", "worker-one", 1, "completed")
            .unwrap();
        let login_terminal = db.dedicated_session("S-one").unwrap().unwrap();
        assert_eq!(login_terminal.state, "terminal");
        assert!(login_terminal.candidate_snapshot_hash.is_none());
    }

    #[test]
    fn dedicated_worker_recovery_retains_prior_boot_and_attaches_next_epoch() {
        let (_tmp, db) = fresh_db();
        create_locked_profile(&db, "P-recover", "worker-recover-1");
        db.conn
            .execute(
                "INSERT INTO execution_workspace (
                    workspace_id, thread_id, launch_owner, backend_id,
                    lower_snapshot, root_path, state, created_at_ms, updated_at_ms
                 ) VALUES ('W-recover', 'T-recover', 'owner', 'backend',
                           'a', '/tmp/workspace-recover', 'ready', 1, 1)",
                [],
            )
            .unwrap();
        db.admit_dedicated_session(NewDedicatedSession {
            session_id: "S-recover",
            root_thread_id: "T-recover",
            owner_principal: "fp:operator",
            admitted_capsule_hash: &"a".repeat(64),
            workspace_id: "W-recover",
            candidate_required: false,
            credential_profile_id: "P-recover",
            credential_generation: 1,
            credential_lock_owner: "worker-recover-1",
        })
        .unwrap();
        let first = WorkerProcessRecord {
            worker_instance_id: "worker-recover-1".to_owned(),
            boot_identity_hash: "b".repeat(64),
            session_capsule_hash: "a".repeat(64),
            boot_epoch: 1,
            lifecycle_generation: 1,
            process_identity: fake_process_identity(131, 131),
            control_channel_identity: "fd:recover-1".to_owned(),
            state: WorkerProcessState::Attached,
            daemon_generation_id: "daemon-one".to_owned(),
            session_id: "S-recover".to_owned(),
            cleanup_state: "owned".to_owned(),
            created_at_ms: 2,
            updated_at_ms: 2,
        };
        db.attach_worker_process(&first).unwrap();
        db.complete_worker_binding("worker-recover-1", "S-recover", 1)
            .unwrap();
        db.bind_dedicated_remote_thread("S-recover", "worker-recover-1", 1, "remote-recover")
            .unwrap();
        db.fence_abandoned_worker_process("worker-recover-1", "S-recover", 1, "reaped")
            .unwrap();

        db.acquire_credential_profile("P-recover", "fp:operator", "worker-recover-2")
            .unwrap();
        assert_eq!(
            db.prepare_dedicated_session_recovery("S-recover", 1, "worker-recover-2")
                .unwrap(),
            2
        );
        let second = WorkerProcessRecord {
            worker_instance_id: "worker-recover-2".to_owned(),
            boot_identity_hash: "c".repeat(64),
            boot_epoch: 2,
            process_identity: fake_process_identity(132, 132),
            control_channel_identity: "fd:recover-2".to_owned(),
            daemon_generation_id: "daemon-two".to_owned(),
            created_at_ms: 3,
            updated_at_ms: 3,
            ..first.clone()
        };
        db.attach_worker_process(&second).unwrap();
        db.complete_worker_binding("worker-recover-2", "S-recover", 2)
            .unwrap();

        let payload = serde_json::json!({"route_id":"session.resume","payload":{}});
        let reattach = db
            .reserve_dedicated_session_command(NewDedicatedSessionCommand {
                session_id: "S-recover",
                idempotency_key: "reattach-two",
                worker_boot_epoch: 2,
                command_kind: "reattach",
                request_digest: &"d".repeat(64),
                payload: &payload,
            })
            .unwrap();
        db.mark_dedicated_command_contacted("S-recover", reattach.command_sequence, 2)
            .unwrap();
        db.settle_dedicated_command(
            "S-recover",
            reattach.command_sequence,
            2,
            true,
            &serde_json::json!({"redacted":true}),
        )
        .unwrap();
        let recovering = db.dedicated_session("S-recover").unwrap().unwrap();
        assert_eq!(recovering.state, "recovering");
        assert_eq!(recovering.send_boundary, "settled");

        let recovered_reattach = db
            .reserve_dedicated_session_command(NewDedicatedSessionCommand {
                session_id: "S-recover",
                idempotency_key: "reattach-recovered-two",
                worker_boot_epoch: 2,
                command_kind: "reattach",
                request_digest: &"e".repeat(64),
                payload: &payload,
            })
            .unwrap();
        db.mark_dedicated_command_contacted("S-recover", recovered_reattach.command_sequence, 2)
            .unwrap();
        db.settle_recovered_dedicated_command(
            "S-recover",
            recovered_reattach.command_sequence,
            2,
            &serde_json::json!({"redacted":true}),
        )
        .unwrap();
        db.observe_dedicated_remote_reattach("S-recover", 2, "remote-recover")
            .unwrap();
        db.settle_dedicated_remote_recovery_status("S-recover", 2, "remote-recover", "idle")
            .unwrap();
        let recovered = db.dedicated_session("S-recover").unwrap().unwrap();
        assert_eq!(recovered.state, "idle");
        assert_eq!(recovered.send_boundary, "settled");

        let terminal_recovered = db
            .reserve_dedicated_session_command(NewDedicatedSessionCommand {
                session_id: "S-recover",
                idempotency_key: "reattach-terminal-two",
                worker_boot_epoch: 2,
                command_kind: "reattach",
                request_digest: &"f".repeat(64),
                payload: &payload,
            })
            .unwrap();
        db.mark_dedicated_command_contacted("S-recover", terminal_recovered.command_sequence, 2)
            .unwrap();
        db.fence_abandoned_worker_process("worker-recover-2", "S-recover", 2, "reaped")
            .unwrap();
        let detached = db.dedicated_session("S-recover").unwrap().unwrap();
        assert_eq!(detached.state, "outcome_unknown");
        assert!(detached.worker_instance_id.is_none());
        assert!(detached.worker_boot_epoch.is_none());
        db.settle_terminal_recovered_dedicated_command(
            "S-recover",
            terminal_recovered.command_sequence,
            2,
            &serde_json::json!({"redacted":true}),
        )
        .unwrap();
        let projected = db
            .reserve_dedicated_session_command(NewDedicatedSessionCommand {
                session_id: "S-recover",
                idempotency_key: "reattach-terminal-two",
                worker_boot_epoch: 2,
                command_kind: "reattach",
                request_digest: &"f".repeat(64),
                payload: &payload,
            })
            .unwrap();
        assert_eq!(projected.state, "completed");
        assert_eq!(
            db.dedicated_session("S-recover").unwrap().unwrap().state,
            "outcome_unknown"
        );

        assert_eq!(
            db.worker_process("worker-recover-1")
                .unwrap()
                .unwrap()
                .cleanup_state,
            "reaped"
        );
        assert_eq!(
            db.worker_process("worker-recover-2")
                .unwrap()
                .unwrap()
                .boot_epoch,
            2
        );
        let history_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM worker_process WHERE session_id='S-recover'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(history_count, 2);
    }

    #[test]
    fn retained_workspace_recovery_is_exact_claim_and_owner_fenced() {
        let (_tmp, db) = fresh_db();
        db.conn
            .execute(
                "INSERT INTO execution_workspace (
                    workspace_id, thread_id, launch_owner, backend_id, backend_version,
                    pinned_root_identities, mount_identity, lower_snapshot, root_path,
                    state, created_at_ms, updated_at_ms
                 ) VALUES ('W-retained', 'T-retained', 'old-owner', 'backend', 'v1',
                           '{}', 'mount', ?1, '/tmp/W-retained', 'ready', 1, 1)",
                [&"a".repeat(64)],
            )
            .unwrap();
        assert!(matches!(
            db.claim_thread_launch("T-retained", "claim-retained", "daemon-new")
                .unwrap(),
            LaunchClaimOutcome::Claimed
        ));
        let recovery_owner = db
            .get_launch_claim("T-retained")
            .unwrap()
            .unwrap()
            .claimed_by;

        assert!(
            db.rebind_workspace_for_recovery(
                "W-retained",
                "T-retained",
                "wrong-old-owner",
                &recovery_owner,
                WorkspaceState::Ready,
                None,
            )
            .is_err()
        );
        db.rebind_workspace_for_recovery(
            "W-retained",
            "T-retained",
            "old-owner",
            &recovery_owner,
            WorkspaceState::Ready,
            None,
        )
        .unwrap();
        let retained = db.workspace("W-retained").unwrap().unwrap();
        assert_eq!(retained.state, WorkspaceState::Ready);
        assert_eq!(
            retained.launch_owner.as_deref(),
            Some(recovery_owner.as_str())
        );
        assert!(retained.process_identity.is_none());

        db.conn
            .execute(
                "INSERT INTO execution_workspace (
                    workspace_id, thread_id, launch_owner, backend_id, backend_version,
                    pinned_root_identities, mount_identity, lower_snapshot,
                    frozen_snapshot_hash, root_path, state, created_at_ms, updated_at_ms
                 ) VALUES ('W-frozen', 'T-frozen', 'old-frozen-owner', 'backend', 'v1',
                           '{}', 'mount', ?1, ?2, '/tmp/W-frozen', 'freezing', 1, 1)",
                rusqlite::params!["b".repeat(64), "c".repeat(64)],
            )
            .unwrap();
        assert!(matches!(
            db.claim_thread_launch("T-frozen", "claim-frozen", "daemon-new")
                .unwrap(),
            LaunchClaimOutcome::Claimed
        ));
        let frozen_recovery_owner = db.get_launch_claim("T-frozen").unwrap().unwrap().claimed_by;
        db.rebind_workspace_for_recovery(
            "W-frozen",
            "T-frozen",
            "old-frozen-owner",
            &frozen_recovery_owner,
            WorkspaceState::Freezing,
            None,
        )
        .unwrap();
        let frozen = db.workspace("W-frozen").unwrap().unwrap();
        assert_eq!(frozen.state, WorkspaceState::Freezing);
        assert_eq!(
            frozen.frozen_snapshot_hash.as_deref(),
            Some("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc")
        );
        assert_eq!(
            frozen.launch_owner.as_deref(),
            Some(frozen_recovery_owner.as_str())
        );
    }

    #[test]
    fn dedicated_worker_attachment_hands_off_an_active_root_workspace_exactly() {
        let (_tmp, db) = fresh_db();
        create_locked_profile(&db, "P-handoff", "worker-handoff");
        let root_identity = serde_json::to_string(&fake_process_identity(124, 124)).unwrap();
        let stale_identity = serde_json::to_string(&fake_process_identity(125, 125)).unwrap();
        db.conn
            .execute(
                "INSERT INTO thread_runtime (thread_id, chain_root_id, process_identity)
                 VALUES ('T-handoff', 'T-handoff', ?1)",
                [&root_identity],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO execution_workspace (
                    workspace_id, thread_id, launch_owner, backend_id,
                    lower_snapshot, root_path, state, process_identity,
                    created_at_ms, updated_at_ms
                 ) VALUES ('W-handoff', 'T-handoff', 'owner', 'backend',
                           'a', '/tmp/workspace-handoff', 'active', ?1, 1, 1)",
                [&stale_identity],
            )
            .unwrap();
        db.admit_dedicated_session(NewDedicatedSession {
            session_id: "S-handoff",
            root_thread_id: "T-handoff",
            owner_principal: "fp:operator",
            admitted_capsule_hash: &"a".repeat(64),
            workspace_id: "W-handoff",
            candidate_required: true,
            credential_profile_id: "P-handoff",
            credential_generation: 1,
            credential_lock_owner: "worker-handoff",
        })
        .unwrap();
        let record = WorkerProcessRecord {
            worker_instance_id: "worker-handoff".to_owned(),
            boot_identity_hash: "b".repeat(64),
            session_capsule_hash: "a".repeat(64),
            boot_epoch: 1,
            lifecycle_generation: 1,
            process_identity: fake_process_identity(126, 126),
            control_channel_identity: "fd:handoff".to_owned(),
            state: WorkerProcessState::Attached,
            daemon_generation_id: "daemon-one".to_owned(),
            session_id: "S-handoff".to_owned(),
            cleanup_state: "owned".to_owned(),
            created_at_ms: 2,
            updated_at_ms: 2,
        };

        assert!(db.attach_worker_process(&record).is_err());
        assert!(db.worker_process("worker-handoff").unwrap().is_none());
        db.conn
            .execute(
                "UPDATE execution_workspace SET process_identity=?2
                  WHERE workspace_id=?1",
                params!["W-handoff", root_identity],
            )
            .unwrap();
        db.attach_worker_process(&record).unwrap();

        let (state, process_identity): (String, String) = db
            .conn
            .query_row(
                "SELECT state, process_identity FROM execution_workspace
                  WHERE workspace_id='W-handoff'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "active");
        assert_eq!(
            process_identity,
            serde_json::to_string(&record.process_identity).unwrap()
        );
    }

    #[test]
    fn credential_revocation_fences_worker_attachment() {
        let (_tmp, db) = fresh_db();
        create_locked_profile(&db, "P-fence", "worker-fence");
        db.conn
            .execute(
                "INSERT INTO execution_workspace (
                    workspace_id, thread_id, launch_owner, backend_id,
                    lower_snapshot, root_path, state, created_at_ms, updated_at_ms
                 ) VALUES ('W-fence', 'T-fence', 'owner', 'backend',
                           'a', '/tmp/workspace-fence', 'ready', 1, 1)",
                [],
            )
            .unwrap();
        db.admit_dedicated_session(NewDedicatedSession {
            session_id: "S-fence",
            root_thread_id: "T-fence",
            owner_principal: "fp:operator",
            admitted_capsule_hash: &"a".repeat(64),
            workspace_id: "W-fence",
            candidate_required: false,
            credential_profile_id: "P-fence",
            credential_generation: 1,
            credential_lock_owner: "worker-fence",
        })
        .unwrap();
        db.revoke_credential_profile("P-fence", "fp:operator", 1)
            .unwrap();
        let now = lillux::time::timestamp_millis() as i64;
        assert!(
            db.attach_worker_process(&WorkerProcessRecord {
                worker_instance_id: "worker-fence".to_owned(),
                boot_identity_hash: "b".repeat(64),
                session_capsule_hash: "a".repeat(64),
                boot_epoch: 1,
                lifecycle_generation: 1,
                process_identity: fake_process_identity(126, 126),
                control_channel_identity: "fd:12".to_owned(),
                state: WorkerProcessState::Attached,
                daemon_generation_id: "daemon-one".to_owned(),
                session_id: "S-fence".to_owned(),
                cleanup_state: "owned".to_owned(),
                created_at_ms: now,
                updated_at_ms: now,
            })
            .is_err()
        );
    }

    #[test]
    fn proved_abandoned_worker_fence_releases_credential_atomically() {
        let (_tmp, db) = fresh_db();
        create_locked_profile(&db, "P-abandoned", "worker-abandoned");
        db.conn
            .execute(
                "INSERT INTO execution_workspace (
                workspace_id, thread_id, launch_owner, backend_id,
                lower_snapshot, root_path, state, created_at_ms, updated_at_ms
             ) VALUES ('W-abandoned', 'T-abandoned', 'owner', 'backend',
                       'a', '/tmp/workspace-abandoned', 'ready', 1, 1)",
                [],
            )
            .unwrap();
        db.admit_dedicated_session(NewDedicatedSession {
            session_id: "S-abandoned",
            root_thread_id: "T-abandoned",
            owner_principal: "fp:operator",
            admitted_capsule_hash: &"a".repeat(64),
            workspace_id: "W-abandoned",
            candidate_required: false,
            credential_profile_id: "P-abandoned",
            credential_generation: 1,
            credential_lock_owner: "worker-abandoned",
        })
        .unwrap();
        let now = lillux::time::timestamp_millis() as i64;
        db.attach_worker_process(&WorkerProcessRecord {
            worker_instance_id: "worker-abandoned".to_owned(),
            boot_identity_hash: "b".repeat(64),
            session_capsule_hash: "a".repeat(64),
            boot_epoch: 1,
            lifecycle_generation: 1,
            process_identity: fake_process_identity(127, 127),
            control_channel_identity: "fd:13".to_owned(),
            state: WorkerProcessState::Attached,
            daemon_generation_id: "daemon-one".to_owned(),
            session_id: "S-abandoned".to_owned(),
            cleanup_state: "owned".to_owned(),
            created_at_ms: now,
            updated_at_ms: now,
        })
        .unwrap();
        db.complete_worker_binding("worker-abandoned", "S-abandoned", 1)
            .unwrap();
        let payload = serde_json::json!({"work":"never-contacted"});
        let committed = db
            .reserve_dedicated_session_command(NewDedicatedSessionCommand {
                session_id: "S-abandoned",
                idempotency_key: "before-crash",
                worker_boot_epoch: 1,
                command_kind: "fixture",
                request_digest: &"d".repeat(64),
                payload: &payload,
            })
            .unwrap();
        assert_eq!(committed.state, "committed");
        db.fence_abandoned_worker_process("worker-abandoned", "S-abandoned", 1, "reaped")
            .unwrap();
        assert_eq!(
            db.credential_profile("P-abandoned")
                .unwrap()
                .unwrap()
                .lock_owner,
            None
        );
        let retry = db
            .reserve_dedicated_session_command(NewDedicatedSessionCommand {
                session_id: "S-abandoned",
                idempotency_key: "before-crash",
                worker_boot_epoch: 2,
                command_kind: "fixture",
                request_digest: &"d".repeat(64),
                payload: &payload,
            })
            .unwrap();
        assert_eq!(retry.state, "failed");
        assert_eq!(
            retry
                .result
                .as_ref()
                .and_then(|value| value.get("retryable_uncontacted"))
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        let outbox = db.dedicated_command_outbox_records().unwrap();
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0], retry);
    }

    #[test]
    fn unproved_abandoned_worker_retains_identity_and_credential_fence() {
        let (_tmp, db) = fresh_db();
        create_locked_profile(&db, "P-unproved", "worker-unproved");
        db.conn
            .execute(
                "INSERT INTO execution_workspace (
                    workspace_id, thread_id, launch_owner, backend_id,
                    lower_snapshot, root_path, state, created_at_ms, updated_at_ms
                 ) VALUES ('W-unproved', 'T-unproved', 'owner', 'backend',
                           'a', '/tmp/workspace-unproved', 'ready', 1, 1)",
                [],
            )
            .unwrap();
        db.admit_dedicated_session(NewDedicatedSession {
            session_id: "S-unproved",
            root_thread_id: "T-unproved",
            owner_principal: "fp:operator",
            admitted_capsule_hash: &"a".repeat(64),
            workspace_id: "W-unproved",
            candidate_required: false,
            credential_profile_id: "P-unproved",
            credential_generation: 1,
            credential_lock_owner: "worker-unproved",
        })
        .unwrap();
        db.attach_worker_process(&WorkerProcessRecord {
            worker_instance_id: "worker-unproved".to_owned(),
            boot_identity_hash: "b".repeat(64),
            session_capsule_hash: "a".repeat(64),
            boot_epoch: 1,
            lifecycle_generation: 1,
            process_identity: fake_process_identity(128, 128),
            control_channel_identity: "fd:14".to_owned(),
            state: WorkerProcessState::Attached,
            daemon_generation_id: "daemon-one".to_owned(),
            session_id: "S-unproved".to_owned(),
            cleanup_state: "owned".to_owned(),
            created_at_ms: 2,
            updated_at_ms: 2,
        })
        .unwrap();
        db.complete_worker_binding("worker-unproved", "S-unproved", 1)
            .unwrap();
        db.fence_abandoned_worker_process("worker-unproved", "S-unproved", 1, "unproved")
            .unwrap();

        let session = db.dedicated_session("S-unproved").unwrap().unwrap();
        assert_eq!(session.state, "outcome_unknown");
        assert_eq!(
            session.worker_instance_id.as_deref(),
            Some("worker-unproved")
        );
        assert_eq!(session.worker_boot_epoch, Some(1));
        assert_eq!(
            db.credential_profile("P-unproved")
                .unwrap()
                .unwrap()
                .lock_owner
                .as_deref(),
            Some("worker-unproved")
        );

        db.settle_worker_process(
            "worker-unproved",
            "S-unproved",
            1,
            "reaped",
            "credential_revoked",
        )
        .unwrap();
        assert_eq!(
            db.worker_process("worker-unproved")
                .unwrap()
                .unwrap()
                .cleanup_state,
            "reaped"
        );
        db.terminalize_dedicated_session("S-unproved", "worker-unproved", 1, "credential_revoked")
            .unwrap();
        assert_eq!(
            db.credential_profile("P-unproved")
                .unwrap()
                .unwrap()
                .lock_owner,
            None
        );
    }

    #[test]
    fn unproved_attachment_failure_persists_exact_worker_and_credential_fence() {
        let (_tmp, db) = fresh_db();
        create_locked_profile(&db, "P-start-unproved", "worker-start-unproved");
        db.conn
            .execute(
                "INSERT INTO execution_workspace (
                    workspace_id, thread_id, launch_owner, backend_id,
                    lower_snapshot, root_path, state, created_at_ms, updated_at_ms
                 ) VALUES ('W-start-unproved', 'T-start-unproved', 'owner', 'backend',
                           'a', '/tmp/workspace-start-unproved', 'ready', 1, 1)",
                [],
            )
            .unwrap();
        db.admit_dedicated_session(NewDedicatedSession {
            session_id: "S-start-unproved",
            root_thread_id: "T-start-unproved",
            owner_principal: "fp:operator",
            admitted_capsule_hash: &"a".repeat(64),
            workspace_id: "W-start-unproved",
            candidate_required: false,
            credential_profile_id: "P-start-unproved",
            credential_generation: 1,
            credential_lock_owner: "worker-start-unproved",
        })
        .unwrap();
        let record = WorkerProcessRecord {
            worker_instance_id: "worker-start-unproved".to_owned(),
            boot_identity_hash: "b".repeat(64),
            session_capsule_hash: "a".repeat(64),
            boot_epoch: 1,
            lifecycle_generation: 1,
            process_identity: fake_process_identity(129, 129),
            control_channel_identity: "fd:15".to_owned(),
            state: WorkerProcessState::Attached,
            daemon_generation_id: "daemon-one".to_owned(),
            session_id: "S-start-unproved".to_owned(),
            cleanup_state: "owned".to_owned(),
            created_at_ms: 2,
            updated_at_ms: 2,
        };
        db.fence_unproved_worker_start(&record, "attach cleanup unproved")
            .unwrap();

        let worker = db.worker_process("worker-start-unproved").unwrap().unwrap();
        assert_eq!(worker.process_identity, record.process_identity);
        assert_eq!(worker.state, WorkerProcessState::Dead);
        assert_eq!(worker.cleanup_state, "unproved");
        let session = db.dedicated_session("S-start-unproved").unwrap().unwrap();
        assert_eq!(session.state, "outcome_unknown");
        assert_eq!(session.send_boundary, "outcome_unknown");
        assert_eq!(
            db.credential_profile("P-start-unproved")
                .unwrap()
                .unwrap()
                .lock_owner
                .as_deref(),
            Some("worker-start-unproved")
        );
        assert!(
            db.fence_unproved_worker_start(
                &WorkerProcessRecord {
                    process_identity: fake_process_identity(130, 130),
                    ..record
                },
                "conflicting retry",
            )
            .is_err()
        );
    }

    #[test]
    fn readiness_failure_terminalizes_a_recovering_dedicated_session() {
        let (_tmp, db) = fresh_db();
        create_locked_profile(&db, "P-ready-fail", "worker-ready-fail");
        db.conn
            .execute(
                "INSERT INTO execution_workspace (
                    workspace_id, thread_id, launch_owner, backend_id,
                    lower_snapshot, root_path, state, created_at_ms, updated_at_ms
                 ) VALUES ('W-ready-fail', 'T-ready-fail', 'owner', 'backend',
                           'a', '/tmp/workspace-ready-fail', 'ready', 1, 1)",
                [],
            )
            .unwrap();
        db.admit_dedicated_session(NewDedicatedSession {
            session_id: "S-ready-fail",
            root_thread_id: "T-ready-fail",
            owner_principal: "fp:operator",
            admitted_capsule_hash: &"a".repeat(64),
            workspace_id: "W-ready-fail",
            candidate_required: false,
            credential_profile_id: "P-ready-fail",
            credential_generation: 1,
            credential_lock_owner: "worker-ready-fail",
        })
        .unwrap();
        db.attach_worker_process(&WorkerProcessRecord {
            worker_instance_id: "worker-ready-fail".to_owned(),
            boot_identity_hash: "b".repeat(64),
            session_capsule_hash: "a".repeat(64),
            boot_epoch: 1,
            lifecycle_generation: 1,
            process_identity: fake_process_identity(125, 125),
            control_channel_identity: "fd:11".to_owned(),
            state: WorkerProcessState::Attached,
            daemon_generation_id: "daemon-one".to_owned(),
            session_id: "S-ready-fail".to_owned(),
            cleanup_state: "owned".to_owned(),
            created_at_ms: 2,
            updated_at_ms: 2,
        })
        .unwrap();
        db.settle_worker_process(
            "worker-ready-fail",
            "S-ready-fail",
            1,
            "reaped",
            "readiness failed",
        )
        .unwrap();
        assert_eq!(
            db.dedicated_session("S-ready-fail").unwrap().unwrap().state,
            "recovering"
        );
        db.fail_dedicated_session_start(
            "S-ready-fail",
            "worker-ready-fail",
            "readiness failed",
            true,
        )
        .unwrap();
        let session = db.dedicated_session("S-ready-fail").unwrap().unwrap();
        assert_eq!(session.state, "terminal");
        assert_eq!(session.terminal_reason.as_deref(), Some("readiness failed"));
        assert_eq!(
            db.credential_profile("P-ready-fail")
                .unwrap()
                .unwrap()
                .lock_owner,
            None
        );
    }

    #[test]
    fn observation_batches_are_epoch_ordered_and_rebuild_only_from_root_facts() {
        let (_tmp, db) = fresh_db();
        db.conn
            .execute(
                "INSERT INTO dedicated_session(
                    session_id, root_thread_id, owner_principal, admitted_capsule_hash,
                    worker_instance_id, worker_boot_epoch, workspace_id,
                    candidate_required, credential_profile_id, credential_generation, remote_thread_id,
                    current_turn_id, state, send_boundary, candidate_snapshot_hash,
                    candidate_validation_hash, publication_result, terminal_reason,
                    created_at_ms, updated_at_ms
                 ) VALUES ('S-observe', 'T-observe', 'fp:operator', ?1,
                           'worker-observe', 3, 'W-observe', 0, 'P-observe', 1,
                           NULL, NULL, 'idle', 'none', NULL, NULL, NULL, NULL, 1, 1)",
                [&"a".repeat(64)],
            )
            .unwrap();
        assert_eq!(
            db.reserve_dedicated_observation_batch("S-observe", 3, 1, 2, None, &"b".repeat(64),)
                .unwrap(),
            ObservationBatchReservation::ContactAppend
        );
        assert_eq!(
            db.reserve_dedicated_observation_batch("S-observe", 3, 1, 2, None, &"b".repeat(64),)
                .unwrap(),
            ObservationBatchReservation::RebuildProjection
        );
        db.settle_dedicated_observation_batch("S-observe", 3, 1, &"b".repeat(64))
            .unwrap();
        assert_eq!(
            db.reserve_dedicated_observation_batch("S-observe", 3, 1, 2, None, &"b".repeat(64),)
                .unwrap(),
            ObservationBatchReservation::AlreadySettled
        );
        assert!(
            db.reserve_dedicated_observation_batch(
                "S-observe",
                3,
                4,
                4,
                Some(&"b".repeat(64)),
                &"c".repeat(64),
            )
            .is_err()
        );
        assert_eq!(
            db.reserve_dedicated_observation_batch(
                "S-observe",
                3,
                3,
                3,
                Some(&"b".repeat(64)),
                &"c".repeat(64),
            )
            .unwrap(),
            ObservationBatchReservation::ContactAppend
        );
        assert!(
            db.reserve_dedicated_observation_batch("S-observe", 2, 1, 1, None, &"d".repeat(64),)
                .is_err()
        );
        let unfinished = db.dedicated_observation_outbox_records().unwrap();
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].first_sequence, 3);
        assert_eq!(unfinished[0].state, "append_contacting");
        db.mark_dedicated_observation_batch_unknown("S-observe", 3, 3, &"c".repeat(64))
            .unwrap();
        assert_eq!(
            db.dedicated_observation_outbox_records().unwrap()[0].state,
            "append_unknown"
        );
        db.discard_unappended_dedicated_observation_batch("S-observe", 3, 3, &"c".repeat(64))
            .unwrap();
        assert!(
            db.dedicated_observation_outbox_records()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn credential_ceremony_and_dedicated_ledgers_are_epoch_fenced() {
        let (_tmp, db) = fresh_db();
        db.create_credential_profile(NewCredentialProfile {
            profile_id: "P-one",
            owner_principal: "fp:operator",
            home_id: "home-one",
        })
        .unwrap();
        assert_eq!(
            db.acquire_credential_profile("P-one", "fp:operator", "login-owner")
                .unwrap(),
            1
        );
        let login_epoch = db
            .begin_credential_enrollment(
                "P-one",
                "login-owner",
                "login-one",
                lillux::time::timestamp_millis() as i64 + 60_000,
            )
            .unwrap();
        let generation = db
            .complete_credential_enrollment(
                "P-one",
                "login-owner",
                "login-one",
                login_epoch,
                &serde_json::json!({"account_label": "fixture"}),
            )
            .unwrap();
        assert_eq!(generation, 2);
        db.release_credential_profile("P-one", "login-owner")
            .unwrap();

        db.conn
            .execute(
                "INSERT INTO execution_workspace (
                    workspace_id, thread_id, launch_owner, backend_id,
                    lower_snapshot, root_path, state, created_at_ms, updated_at_ms
                 ) VALUES ('W-ledger', 'T-ledger', 'dedicated_worker_session', 'trusted-daemon',
                           'a', '/tmp/workspace-ledger', 'ready', 1, 1)",
                [],
            )
            .unwrap();
        assert_eq!(
            db.acquire_credential_profile("P-one", "fp:operator", "worker-ledger")
                .unwrap(),
            generation
        );
        db.admit_dedicated_session(NewDedicatedSession {
            session_id: "S-ledger",
            root_thread_id: "T-ledger",
            owner_principal: "fp:operator",
            admitted_capsule_hash: &"c".repeat(64),
            workspace_id: "W-ledger",
            candidate_required: false,
            credential_profile_id: "P-one",
            credential_generation: generation,
            credential_lock_owner: "worker-ledger",
        })
        .unwrap();
        let now = lillux::time::timestamp_millis() as i64;
        db.attach_worker_process(&WorkerProcessRecord {
            worker_instance_id: "worker-ledger".to_owned(),
            boot_identity_hash: "b".repeat(64),
            session_capsule_hash: "c".repeat(64),
            boot_epoch: 4,
            lifecycle_generation: 1,
            process_identity: fake_process_identity(124, 124),
            control_channel_identity: "fd:10".to_owned(),
            state: WorkerProcessState::Attached,
            daemon_generation_id: "daemon-one".to_owned(),
            session_id: "S-ledger".to_owned(),
            cleanup_state: "owned".to_owned(),
            created_at_ms: now,
            updated_at_ms: now,
        })
        .unwrap();
        db.complete_worker_binding("worker-ledger", "S-ledger", 4)
            .unwrap();

        let payload = serde_json::json!({"operation": "fixture_turn"});
        let command = db
            .reserve_dedicated_session_command(NewDedicatedSessionCommand {
                session_id: "S-ledger",
                idempotency_key: "request-one",
                worker_boot_epoch: 4,
                command_kind: "request",
                request_digest: &"d".repeat(64),
                payload: &payload,
            })
            .unwrap();
        let replay = db
            .reserve_dedicated_session_command(NewDedicatedSessionCommand {
                session_id: "S-ledger",
                idempotency_key: "request-one",
                worker_boot_epoch: 4,
                command_kind: "request",
                request_digest: &"d".repeat(64),
                payload: &payload,
            })
            .unwrap();
        assert_eq!(command, replay);
        db.mark_dedicated_command_contacted("S-ledger", command.command_sequence, 4)
            .unwrap();
        db.observe_dedicated_session_state(
            "S-ledger",
            4,
            "idle",
            "turn_running",
            None,
            Some("turn-fixture"),
        )
        .unwrap();
        db.create_dedicated_session_approval(NewDedicatedSessionApproval {
            session_id: "S-ledger",
            approval_id: "approval-one",
            worker_instance_id: "worker-ledger",
            worker_boot_epoch: 4,
            request_digest: &"e".repeat(64),
            operation_class: "fixture",
            requested_authority: &serde_json::json!({}),
            expires_at_ms: lillux::time::timestamp_millis() as i64 + 60_000,
        })
        .unwrap();
        let approval_decision = serde_json::json!({"decision": "deny"});
        let approval_decision_digest =
            ryeos_state::objects::canonical_value_digest(&approval_decision).unwrap();
        db.reserve_dedicated_session_approval_decision(
            "S-ledger",
            "approval-one",
            4,
            &"e".repeat(64),
            "fp:operator",
            &approval_decision,
            &approval_decision_digest,
            "reservation-one",
        )
        .unwrap();
        assert!(
            db.reserve_dedicated_session_approval_decision(
                "S-ledger",
                "approval-one",
                4,
                &"e".repeat(64),
                "fp:operator",
                &approval_decision,
                &approval_decision_digest,
                "reservation-two",
            )
            .is_err()
        );
        db.mark_dedicated_approval_delivery_contacting(
            "S-ledger",
            "approval-one",
            4,
            "reservation-one",
            &approval_decision_digest,
        )
        .unwrap();
        db.settle_dedicated_approval_delivery(
            "S-ledger",
            "approval-one",
            4,
            "reservation-one",
            &approval_decision_digest,
        )
        .unwrap();
        db.conn
            .execute(
                "INSERT INTO dedicated_session_approval (
                    session_id, approval_id, worker_instance_id, worker_boot_epoch,
                    request_digest, operation_class, requested_authority_json, state,
                    decision_principal, decision_json, decision_digest, reservation_token,
                    expires_at_ms, created_at_ms, resolved_at_ms,
                    delivery_contacted_at_ms, delivery_settled_at_ms
                 ) VALUES ('S-ledger', 'approval-recovered', 'worker-ledger', 4,
                           ?1, 'fixture', '{}', 'delivery_unknown', 'fp:operator',
                           ?2, ?3, 'reservation-recovered', ?4, 1, 1, 1, NULL)",
                params![
                    "9".repeat(64),
                    serde_json::to_string(&approval_decision).unwrap(),
                    approval_decision_digest,
                    lillux::time::timestamp_millis() as i64 + 60_000,
                ],
            )
            .unwrap();
        db.settle_recovered_dedicated_approval_delivery(
            "S-ledger",
            "approval-recovered",
            4,
            "reservation-recovered",
            &approval_decision_digest,
        )
        .unwrap();
        // Root-derived projection repair is idempotent and does not depend on
        // the session still retaining this historical worker epoch.
        db.settle_recovered_dedicated_approval_delivery(
            "S-ledger",
            "approval-recovered",
            4,
            "reservation-recovered",
            &approval_decision_digest,
        )
        .unwrap();
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT state FROM dedicated_session_approval
                      WHERE session_id='S-ledger' AND approval_id='approval-recovered'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "delivery_settled"
        );
        db.create_dedicated_session_approval(NewDedicatedSessionApproval {
            session_id: "S-ledger",
            approval_id: "approval-uncontacted",
            worker_instance_id: "worker-ledger",
            worker_boot_epoch: 4,
            request_digest: &"8".repeat(64),
            operation_class: "fixture",
            requested_authority: &serde_json::json!({}),
            expires_at_ms: lillux::time::timestamp_millis() as i64 + 60_000,
        })
        .unwrap();
        db.reserve_dedicated_session_approval_decision(
            "S-ledger",
            "approval-uncontacted",
            4,
            &"8".repeat(64),
            "fp:operator",
            &approval_decision,
            &approval_decision_digest,
            "reservation-uncontacted",
        )
        .unwrap();
        db.reconcile_dedicated_approval_stale_epoch("S-ledger", "approval-uncontacted", 4)
            .unwrap();
        db.reconcile_dedicated_approval_stale_epoch("S-ledger", "approval-uncontacted", 4)
            .unwrap();
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT state FROM dedicated_session_approval
                      WHERE session_id='S-ledger' AND approval_id='approval-uncontacted'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "stale_epoch"
        );
        db.settle_dedicated_command(
            "S-ledger",
            command.command_sequence,
            4,
            true,
            &serde_json::json!({"ok": true}),
        )
        .unwrap();
        db.observe_dedicated_session_state(
            "S-ledger",
            4,
            "turn_running",
            "idle",
            Some("turn-fixture"),
            None,
        )
        .unwrap();
        assert_eq!(
            db.dedicated_session("S-ledger").unwrap().unwrap().state,
            "idle"
        );
        let recovered_command = db
            .reserve_dedicated_session_command(NewDedicatedSessionCommand {
                session_id: "S-ledger",
                idempotency_key: "request-recovered",
                worker_boot_epoch: 4,
                command_kind: "request",
                request_digest: &"c".repeat(64),
                payload: &payload,
            })
            .unwrap();
        db.mark_dedicated_command_contacted("S-ledger", recovered_command.command_sequence, 4)
            .unwrap();
        db.mark_dedicated_command_outcome_unknown(
            "S-ledger",
            recovered_command.command_sequence,
            4,
        )
        .unwrap();
        db.settle_recovered_dedicated_command(
            "S-ledger",
            recovered_command.command_sequence,
            4,
            &serde_json::json!({"redacted":true,"response_digest":"c".repeat(64)}),
        )
        .unwrap();
        let recovered = db
            .reserve_dedicated_session_command(NewDedicatedSessionCommand {
                session_id: "S-ledger",
                idempotency_key: "request-recovered",
                worker_boot_epoch: 4,
                command_kind: "request",
                request_digest: &"c".repeat(64),
                payload: &payload,
            })
            .unwrap();
        assert_eq!(recovered.state, "completed");
        let session = db.dedicated_session("S-ledger").unwrap().unwrap();
        assert_eq!(session.state, "idle");
        assert_eq!(session.send_boundary, "settled");
        let expiry_command = db
            .reserve_dedicated_session_command(NewDedicatedSessionCommand {
                session_id: "S-ledger",
                idempotency_key: "request-expiry",
                worker_boot_epoch: 4,
                command_kind: "events",
                request_digest: &"a".repeat(64),
                payload: &payload,
            })
            .unwrap();
        db.mark_dedicated_command_contacted("S-ledger", expiry_command.command_sequence, 4)
            .unwrap();
        db.observe_dedicated_session_state(
            "S-ledger",
            4,
            "idle",
            "turn_running",
            None,
            Some("turn-expiry"),
        )
        .unwrap();
        db.create_dedicated_session_approval(NewDedicatedSessionApproval {
            session_id: "S-ledger",
            approval_id: "approval-expiry",
            worker_instance_id: "worker-ledger",
            worker_boot_epoch: 4,
            request_digest: &"b".repeat(64),
            operation_class: "fixture",
            requested_authority: &serde_json::json!({}),
            expires_at_ms: lillux::time::timestamp_millis() as i64 + 60_000,
        })
        .unwrap();
        db.conn
            .execute(
                "UPDATE dedicated_session_approval SET expires_at_ms=?1
                  WHERE session_id='S-ledger' AND approval_id='approval-expiry'",
                [lillux::time::timestamp_millis() as i64 - 1],
            )
            .unwrap();
        db.expire_dedicated_session_approval("S-ledger", "approval-expiry", 4)
            .unwrap();
        assert!(
            db.pending_dedicated_session_approvals("S-ledger")
                .unwrap()
                .is_empty()
        );
        db.settle_dedicated_command(
            "S-ledger",
            expiry_command.command_sequence,
            4,
            true,
            &serde_json::json!({"expired":true}),
        )
        .unwrap();
        db.observe_dedicated_session_state(
            "S-ledger",
            4,
            "turn_running",
            "idle",
            Some("turn-expiry"),
            None,
        )
        .unwrap();
        assert_eq!(
            db.revoke_credential_profile("P-one", "fp:operator", generation)
                .unwrap(),
            3
        );
        assert!(
            db.reserve_dedicated_session_command(NewDedicatedSessionCommand {
                session_id: "S-ledger",
                idempotency_key: "request-after-revocation",
                worker_boot_epoch: 4,
                command_kind: "request",
                request_digest: &"f".repeat(64),
                payload: &payload,
            })
            .is_err()
        );
    }

    #[test]
    fn frozen_candidate_requires_exact_verification_before_publish_or_discard() {
        let (_tmp, db) = fresh_db();
        create_locked_profile(&db, "P-candidate", "worker-candidate");
        db.conn
            .execute(
                "INSERT INTO execution_workspace (
                    workspace_id, thread_id, launch_owner, backend_id,
                    lower_snapshot, root_path, state, created_at_ms, updated_at_ms
                 ) VALUES ('W-candidate', 'S-candidate', 'owner', 'trusted-daemon',
                           'a', '/tmp/candidate', 'ready', 1, 1)",
                [],
            )
            .unwrap();
        db.admit_dedicated_session(NewDedicatedSession {
            session_id: "S-candidate",
            root_thread_id: "S-candidate",
            owner_principal: "fp:operator",
            admitted_capsule_hash: &"a".repeat(64),
            workspace_id: "W-candidate",
            candidate_required: true,
            credential_profile_id: "P-candidate",
            credential_generation: 1,
            credential_lock_owner: "worker-candidate",
        })
        .unwrap();
        let now = lillux::time::timestamp_millis() as i64;
        db.attach_worker_process(&WorkerProcessRecord {
            worker_instance_id: "worker-candidate".to_owned(),
            boot_identity_hash: "b".repeat(64),
            session_capsule_hash: "a".repeat(64),
            boot_epoch: 1,
            lifecycle_generation: 1,
            process_identity: fake_process_identity(125, 125),
            control_channel_identity: "fd:11".to_owned(),
            state: WorkerProcessState::Attached,
            daemon_generation_id: "daemon-one".to_owned(),
            session_id: "S-candidate".to_owned(),
            cleanup_state: "owned".to_owned(),
            created_at_ms: now,
            updated_at_ms: now,
        })
        .unwrap();
        db.complete_worker_binding("worker-candidate", "S-candidate", 1)
            .unwrap();
        db.reserve_dedicated_session_completion("S-candidate", 1)
            .unwrap();
        db.settle_worker_process("worker-candidate", "S-candidate", 1, "reaped", "completed")
            .unwrap();
        db.terminalize_dedicated_session("S-candidate", "worker-candidate", 1, "completed")
            .unwrap();
        db.conn
            .execute(
                "UPDATE execution_workspace SET state='closed'
                  WHERE workspace_id='W-candidate'",
                [],
            )
            .unwrap();
        let candidate = "c".repeat(64);
        assert!(
            db.bind_dedicated_session_candidate("S-candidate", &candidate)
                .unwrap()
        );
        let frozen = db.dedicated_session("S-candidate").unwrap().unwrap();
        assert_eq!(frozen.state, "frozen");
        let plan = frozen.candidate_validation_hash.unwrap();
        assert!(
            db.reserve_dedicated_candidate_publication("S-candidate", &candidate)
                .is_err()
        );
        assert!(
            db.reserve_dedicated_candidate_validation("S-candidate", &candidate, &"d".repeat(64))
                .is_err()
        );
        db.reserve_dedicated_candidate_validation("S-candidate", &candidate, &plan)
            .unwrap();
        db.fail_dedicated_candidate_disposition("S-candidate", "verifying")
            .unwrap();
        assert_eq!(
            db.dedicated_session("S-candidate").unwrap().unwrap().state,
            "frozen"
        );
        db.reserve_dedicated_candidate_validation("S-candidate", &candidate, &plan)
            .unwrap();
        db.settle_dedicated_candidate_validation(
            "S-candidate",
            &candidate,
            &plan,
            &serde_json::json!({"ok":true}),
        )
        .unwrap();
        assert_eq!(
            db.dedicated_session("S-candidate").unwrap().unwrap().state,
            "publish_ready"
        );
        db.reserve_dedicated_candidate_discard("S-candidate", &candidate)
            .unwrap();
        db.settle_dedicated_candidate_discard("S-candidate", &candidate)
            .unwrap();
        let discarded = db.dedicated_session("S-candidate").unwrap().unwrap();
        assert_eq!(discarded.state, "terminal");
        assert_eq!(discarded.publication_result.as_deref(), Some("discarded"));
    }

    #[test]
    fn in_process_birth_reservation_is_atomic_typed_and_settleable() {
        let (_tmp, db) = fresh_db();
        db.reserve_in_process_handler_birth(
            "T-service",
            "T-service",
            &in_process_launch_metadata(),
        )
        .unwrap();
        assert_eq!(
            db.in_process_handler_reservations_after(None, IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE,)
                .unwrap(),
            [InProcessHandlerReservation {
                thread_id: "T-service".to_string(),
                phase: InProcessHandlerReservationPhase::Pending,
            }]
        );
        assert!(db.get_runtime_info("T-service").unwrap().is_some());
        let attach_error = db
            .attach_process(
                "T-service",
                1234,
                1234,
                &fake_process_identity(1234, 1234),
                &RuntimeLaunchMetadata::default(),
            )
            .unwrap_err();
        assert!(
            attach_error
                .to_string()
                .contains("cannot attach an external process")
        );

        db.mark_in_process_handler_birth_running("T-service")
            .unwrap();
        db.mark_in_process_handler_birth_running("T-service")
            .unwrap();
        assert_eq!(
            db.in_process_handler_reservations_after(None, IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE,)
                .unwrap()[0]
                .phase,
            InProcessHandlerReservationPhase::Running
        );
        let pins = db
            .inspect_chain_recovery_pins("T-service", &["T-service".to_string()])
            .unwrap();
        assert_eq!(pins.in_process_handler_reservations, 1);
        assert!(!pins.is_empty());
        assert!(
            db.settle_in_process_handler_reservation("T-service")
                .unwrap()
        );
        assert!(
            db.settle_in_process_handler_reservation("T-service")
                .unwrap()
        );
        assert_eq!(
            db.in_process_handler_reservations_after(None, IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE,)
                .unwrap()[0]
                .phase,
            InProcessHandlerReservationPhase::TerminalConfirmed
        );
        assert!(
            db.delete_terminal_in_process_handler_reservation("T-service")
                .unwrap()
        );
        assert!(
            db.in_process_handler_reservations_after(None, IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE,)
                .unwrap()
                .is_empty()
        );
        assert!(
            db.get_runtime_info("T-service").unwrap().is_some(),
            "terminal settlement retains historical runtime diagnostics"
        );
        assert!(
            db.inspect_chain_recovery_pins("T-service", &["T-service".to_string()])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn pending_in_process_birth_cleanup_removes_both_rows_and_running_refuses_cleanup() {
        let (_tmp, db) = fresh_db();
        db.reserve_in_process_handler_birth(
            "T-pending",
            "T-pending",
            &in_process_launch_metadata(),
        )
        .unwrap();
        assert!(
            db.discard_pending_in_process_handler_birth("T-pending")
                .unwrap()
        );
        assert!(db.get_runtime_info("T-pending").unwrap().is_none());
        assert!(
            db.in_process_handler_reservations_after(None, IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE,)
                .unwrap()
                .is_empty()
        );

        db.reserve_in_process_handler_birth(
            "T-running",
            "T-running",
            &in_process_launch_metadata(),
        )
        .unwrap();
        db.mark_in_process_handler_birth_running("T-running")
            .unwrap();
        let error = db
            .discard_pending_in_process_handler_birth("T-running")
            .unwrap_err();
        assert!(error.to_string().contains("refusing to discard"));
        assert!(db.get_runtime_info("T-running").unwrap().is_some());
    }

    #[test]
    fn runtime_schema_without_in_process_reservation_contract_rejects_without_cutover_advice() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let db = RuntimeDb::open(&path).unwrap();
        db.conn
            .execute_batch(
                "DROP INDEX idx_in_process_handler_reservation_phase_thread;
                 DROP TABLE in_process_handler_reservation;",
            )
            .unwrap();
        drop(db);

        let error = RuntimeDb::open(&path)
            .err()
            .expect("missing current reservation contract must fail closed");
        assert!(
            !format!("{error:#}")
                .contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND)
        );
        let read_only =
            Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let table_count: i64 = read_only
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                  WHERE type = 'table' AND name = 'in_process_handler_reservation'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 0, "ordinary open must not migrate the store");
    }

    #[test]
    fn generic_launch_metadata_writer_cannot_bypass_in_process_reservation() {
        let (_tmp, db) = fresh_db();
        db.insert_thread_runtime("T-generic", "T-generic").unwrap();
        let error = db
            .set_launch_metadata("T-generic", &in_process_launch_metadata())
            .unwrap_err();
        assert!(error.to_string().contains("atomic birth reservation"));
        assert!(
            db.get_runtime_info("T-generic")
                .unwrap()
                .unwrap()
                .launch_metadata
                .is_none()
        );
        assert!(
            db.in_process_handler_reservations_after(None, IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE,)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn project_authority_envelope_roundtrips_exact_current_contract() {
        let authority = ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS;
        let raw = encode_current_project_authority(&authority).unwrap();
        let value: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["kind"], PROJECT_AUTHORITY_ENVELOPE_KIND);
        assert_eq!(
            value["schema_epoch"],
            Value::from(PROJECT_AUTHORITY_SCHEMA_EPOCH)
        );
        assert_eq!(decode_current_project_authority(&raw).unwrap(), authority);
    }

    #[test]
    fn predecessor_project_authority_envelope_is_rejected_before_nested_decode() {
        let raw = lillux::canonical_json(&serde_json::json!({
            "kind": PROJECT_AUTHORITY_ENVELOPE_KIND,
            "schema_epoch": PROJECT_AUTHORITY_SCHEMA_EPOCH - 1,
            "authority": {
                "kind": "pinned_generation",
                "original_project_path": "/project",
                "project_identity": "project",
                "snapshot_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "persistence": "copy_on_write",
                "access": "read_write",
                "authorized_write_namespaces": ["project"],
                "confinement": {
                    "denied_control_paths": [".ai"],
                    "symlink_policy": "descriptor_rooted_no_escape"
                }
            }
        }))
        .unwrap();
        let error = decode_current_project_authority(&raw).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("stored project authority is not the exact current contract"));
        assert!(message.contains(&format!(
            "current schema_epoch={PROJECT_AUTHORITY_SCHEMA_EPOCH}"
        )));
        assert!(message.contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND));
        assert!(!message.contains("missing field `base_snapshot_hash`"));
    }

    #[test]
    fn newer_project_authority_envelope_rejects_without_destructive_cutover_advice() {
        let raw = lillux::canonical_json(&serde_json::json!({
            "kind": PROJECT_AUTHORITY_ENVELOPE_KIND,
            "schema_epoch": PROJECT_AUTHORITY_SCHEMA_EPOCH + 1,
            "authority": {"deliberately": "newer"}
        }))
        .unwrap();

        let error = decode_current_project_authority(&raw).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("not the exact current contract"));
        assert!(message.contains(&format!(
            "stored schema_epoch={}",
            PROJECT_AUTHORITY_SCHEMA_EPOCH + 1
        )));
        assert!(
            !message.contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND)
        );
        assert!(!requires_execution_schema_cutover(&error));
        assert!(is_newer_execution_schema(&error));
    }

    #[test]
    fn newer_runtime_operator_epoch_refuses_open_and_explicit_reset() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        drop(RuntimeDb::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        let newer_application_id =
            RUNTIME_OPERATOR_APP_ID_PREFIX | (RUNTIME_OPERATOR_SCHEMA_EPOCH + 1);
        conn.pragma_update(None, "application_id", newer_application_id)
            .unwrap();
        drop(conn);

        let error = RuntimeDb::open(&path)
            .err()
            .expect("an older daemon must reject a newer runtime store");
        let message = format!("{error:#}");
        assert!(message.contains(&format!(
            "stored schema_epoch={}",
            RUNTIME_OPERATOR_SCHEMA_EPOCH + 1
        )));
        assert!(
            !message.contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND)
        );

        let reset_error = RuntimeDb::open_for_explicit_history_reset(&path)
            .err()
            .expect("explicit reset must not erase a newer runtime store");
        assert!(format!("{reset_error:#}").contains("refusing destructive reset"));
        assert_eq!(
            Connection::open(&path)
                .unwrap()
                .query_row("PRAGMA application_id", [], |row| row.get::<_, u32>(0))
                .unwrap(),
            newer_application_id
        );
    }

    #[test]
    fn v1_hook_dispatch_epoch_refuses_ordinary_open_and_requires_explicit_reset() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        drop(RuntimeDb::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "application_id", RUNTIME_OPERATOR_APP_ID_PREFIX | 1)
            .unwrap();
        drop(conn);

        let error = RuntimeDb::open(&path)
            .err()
            .expect("v1 hook dispatch authority must not open under v2");
        let message = format!("{error:#}");
        assert!(message.contains("stored schema_epoch=1"));
        assert!(requires_execution_schema_cutover(&error), "{message}");
        assert!(
            message.contains("explicit no-backcompat reset"),
            "{message}"
        );

        let inspection = RuntimeDb::open_for_explicit_history_reset(&path)
            .expect("a proven predecessor is reset-eligible, never migrated");
        assert!(inspection.requires_explicit_history_reset());
    }

    #[test]
    fn unowned_database_is_never_classified_as_a_predecessor() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE foreign_state(value TEXT);")
            .unwrap();
        drop(conn);

        let error = RuntimeDb::open(&path)
            .err()
            .expect("an unowned database must fail closed");
        let message = format!("{error:#}");
        assert!(message.contains("refusing to classify or reset unowned store"));
        assert!(
            !message.contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND)
        );

        let reset_error = RuntimeDb::open_for_explicit_history_reset(&path)
            .err()
            .expect("explicit reset must not erase an unowned database");
        assert!(format!("{reset_error:#}").contains("unowned store"));
        assert_eq!(
            Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                      WHERE type='table' AND name='foreign_state'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn other_ryeos_database_ids_are_outside_the_runtime_operator_family() {
        let tmp = TempDir::new().unwrap();
        for (label, application_id) in [
            ("scheduler", 0x5259_5343),
            ("operational", 0x5259_4f50),
            ("projection", 0x5259_504a),
            ("accounting", 0x5259_4143),
        ] {
            let path = tmp.path().join(format!("{label}.sqlite3"));
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE foreign_state(value TEXT);")
                .unwrap();
            conn.pragma_update(None, "application_id", application_id)
                .unwrap();
            drop(conn);

            let error = RuntimeDb::open_for_explicit_history_reset(&path)
                .err()
                .expect("another RyeOS database must remain unowned runtime state");
            assert!(format!("{error:#}").contains("unowned store"));
            assert_eq!(
                Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .unwrap()
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master
                          WHERE type='table' AND name='foreign_state'",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                1
            );
        }
    }

    #[test]
    fn newer_project_authority_vetoes_mixed_epoch_reset() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let db = RuntimeDb::open(&path).unwrap();
        let predecessor = lillux::canonical_json(&serde_json::json!({
            "kind": PROJECT_AUTHORITY_ENVELOPE_KIND,
            "schema_epoch": PROJECT_AUTHORITY_SCHEMA_EPOCH - 1,
            "authority": {"deliberately": "predecessor"}
        }))
        .unwrap();
        let newer = lillux::canonical_json(&serde_json::json!({
            "kind": PROJECT_AUTHORITY_ENVELOPE_KIND,
            "schema_epoch": PROJECT_AUTHORITY_SCHEMA_EPOCH + 1,
            "authority": {"deliberately": "newer"}
        }))
        .unwrap();
        for (operation_id, authority) in [
            ("a-predecessor", predecessor.as_str()),
            ("z-newer", newer.as_str()),
        ] {
            db.conn
                .execute(
                    "INSERT INTO detached_spawn_intent(
                         operation_id, parent_thread_id, request_hash, child_thread_id,
                         child_project_authority, created_at_ms
                     ) VALUES (?1, 'T-parent', ?1, ?1, ?2, 1)",
                    params![operation_id, authority],
                )
                .unwrap();
        }
        db.conn
            .pragma_update(None, "application_id", PREDECESSOR_RUNTIME_APP_ID)
            .expect("mark the mixed store as predecessor-layout");
        drop(db);

        let error = RuntimeDb::open_for_explicit_history_reset(&path)
            .err()
            .expect("a newer row must veto reset even after a predecessor row");
        let message = format!("{error:#}");
        assert!(message.contains("z-newer"));
        assert!(message.contains("newer"));
        assert!(
            !message.contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND)
        );

        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let retained: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM detached_spawn_intent
                  WHERE operation_id IN ('a-predecessor', 'z-newer')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, 2);
    }

    #[test]
    fn newer_capsule_epoch_vetoes_reset_even_without_a_current_sealed_request() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let db = RuntimeDb::open(&path).unwrap();
        let predecessor = lillux::canonical_json(&serde_json::json!({
            "kind": PROJECT_AUTHORITY_ENVELOPE_KIND,
            "schema_epoch": PROJECT_AUTHORITY_SCHEMA_EPOCH - 1,
            "authority": {"deliberately": "predecessor"}
        }))
        .unwrap();
        db.conn
            .execute(
                "INSERT INTO detached_spawn_intent(
                     operation_id, parent_thread_id, request_hash, child_thread_id,
                     child_project_authority, created_at_ms
                 ) VALUES ('a-predecessor', 'T-parent', 'request', 'T-child', ?1, 1)",
                params![predecessor],
            )
            .unwrap();
        db.insert_thread_runtime("T-newer-capsule", "T-newer-capsule")
            .unwrap();
        let newer_capsule = lillux::canonical_json(&serde_json::json!({
            "schema_version": LAUNCH_METADATA_SCHEMA_VERSION,
            "admitted_launch_capsule_schema":
                ryeos_state::objects::ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION + 1,
            "sealed_root_request": null
        }))
        .unwrap();
        db.conn
            .execute(
                "UPDATE thread_runtime SET launch_metadata=?1 WHERE thread_id='T-newer-capsule'",
                params![newer_capsule],
            )
            .unwrap();
        db.conn
            .pragma_update(None, "application_id", PREDECESSOR_RUNTIME_APP_ID)
            .expect("mark the mixed store as predecessor-layout");
        drop(db);

        let error = RuntimeDb::open_for_explicit_history_reset(&path)
            .err()
            .expect("a newer nested capsule epoch must veto reset");
        let message = format!("{error:#}");
        assert!(message.contains("T-newer-capsule"));
        assert!(message.contains("newer admitted launch capsule"));
        assert!(
            !message.contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND)
        );
    }

    #[test]
    fn unrecognized_project_authority_kind_vetoes_mixed_epoch_reset() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let db = RuntimeDb::open(&path).unwrap();
        let predecessor = lillux::canonical_json(&serde_json::json!({
            "kind": PROJECT_AUTHORITY_ENVELOPE_KIND,
            "schema_epoch": PROJECT_AUTHORITY_SCHEMA_EPOCH - 1,
            "authority": {"deliberately": "predecessor"}
        }))
        .unwrap();
        let unrecognized = lillux::canonical_json(&serde_json::json!({
            "kind": "future_project_authority",
            "authority": {"deliberately": "opaque"}
        }))
        .unwrap();
        for (operation_id, authority) in [
            ("a-predecessor", predecessor.as_str()),
            ("z-unrecognized", unrecognized.as_str()),
        ] {
            db.conn
                .execute(
                    "INSERT INTO detached_spawn_intent(
                         operation_id, parent_thread_id, request_hash, child_thread_id,
                         child_project_authority, created_at_ms
                     ) VALUES (?1, 'T-parent', ?1, ?1, ?2, 1)",
                    params![operation_id, authority],
                )
                .unwrap();
        }
        db.conn
            .pragma_update(None, "application_id", PREDECESSOR_RUNTIME_APP_ID)
            .expect("mark the mixed store as predecessor-layout");
        drop(db);

        let error = RuntimeDb::open_for_explicit_history_reset(&path)
            .err()
            .expect("an unrecognized authority kind must veto reset");
        let message = format!("{error:#}");
        assert!(message.contains("z-unrecognized"));
        assert!(message.contains("unrecognized outer kind"));
        assert!(
            !message.contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND)
        );

        let retained: i64 = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM detached_spawn_intent
                  WHERE operation_id IN ('a-predecessor', 'z-unrecognized')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, 2);
    }

    #[test]
    fn explicit_history_reset_refuses_newer_project_authority() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let db = RuntimeDb::open(&path).unwrap();
        let newer = lillux::canonical_json(&serde_json::json!({
            "kind": PROJECT_AUTHORITY_ENVELOPE_KIND,
            "schema_epoch": PROJECT_AUTHORITY_SCHEMA_EPOCH + 1,
            "authority": {"deliberately": "newer"}
        }))
        .unwrap();
        db.conn
            .execute(
                "INSERT INTO detached_spawn_intent(
                     operation_id, parent_thread_id, request_hash, child_thread_id,
                     child_project_authority, created_at_ms
                 ) VALUES ('op-newer', 'T-parent', 'request', 'T-child', ?1, 1)",
                params![newer],
            )
            .unwrap();
        drop(db);

        let error = RuntimeDb::open_for_explicit_history_reset(&path)
            .err()
            .expect("an older daemon must not erase newer runtime authority");
        let message = format!("{error:#}");
        assert!(message.contains("newer than this RyeOS binary"));
        assert!(
            !message.contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND)
        );

        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let retained: String = conn
            .query_row(
                "SELECT child_project_authority
                   FROM detached_spawn_intent
                  WHERE operation_id='op-newer'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, newer);
    }

    #[test]
    fn existing_reset_inspection_classifies_predecessor_without_mutation() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let db = RuntimeDb::open(&path).unwrap();
        let predecessor = lillux::canonical_json(&serde_json::json!({
            "kind": PROJECT_AUTHORITY_ENVELOPE_KIND,
            "schema_epoch": PROJECT_AUTHORITY_SCHEMA_EPOCH - 1,
            "authority": {"deliberately": "predecessor"}
        }))
        .unwrap();
        db.conn
            .execute(
                "INSERT INTO detached_spawn_intent(
                     operation_id, parent_thread_id, request_hash, child_thread_id,
                     child_project_authority, created_at_ms
                 ) VALUES ('op-predecessor', 'T-parent', 'request', 'T-child', ?1, 1)",
                params![predecessor],
            )
            .unwrap();
        drop(db);

        let source_entries = || {
            std::fs::read_dir(tmp.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<BTreeSet<_>>()
        };
        let before_entries = source_entries();
        let mut inspection = RuntimeDb::open_existing_for_explicit_history_reset(&path)
            .expect("dry-run inspection must classify the predecessor store");
        assert!(inspection.requires_explicit_history_reset());
        assert!(
            inspection
                .apply_explicit_history_reset(&path)
                .unwrap_err()
                .to_string()
                .contains("inspection authority")
        );
        assert_eq!(source_entries(), before_entries);
        drop(inspection);
        assert_eq!(source_entries(), before_entries);

        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let retained: String = conn
            .query_row(
                "SELECT child_project_authority
                   FROM detached_spawn_intent
                  WHERE operation_id='op-predecessor'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, predecessor);
    }

    #[test]
    fn predecessor_launch_contract_is_rejected_before_nested_authority_decode() {
        let raw = lillux::canonical_json(&serde_json::json!({
            "schema_version": LAUNCH_METADATA_SCHEMA_VERSION - 1,
            "resume_context": {
                "project_authority": {
                    "kind": "live_project",
                    "live_access": {
                        "access": "read_write",
                        "authorized_write_namespaces": ["project"],
                        "denied_control_paths": [".ai"],
                        "symlink_policy": "descriptor_rooted_no_escape"
                    }
                }
            }
        }))
        .unwrap();

        let error = decode_current_launch_metadata(&raw).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("not the exact current contract"));
        assert!(message.contains(&format!(
            "stored schema_version={}",
            LAUNCH_METADATA_SCHEMA_VERSION - 1
        )));
        assert!(message.contains(&format!(
            "current schema_version={LAUNCH_METADATA_SCHEMA_VERSION}"
        )));
        assert!(message.contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND));
        assert!(!message.contains("missing field `confinement`"));
        assert!(!message.contains("unknown field"));
    }

    #[test]
    fn newer_launch_contract_rejects_without_destructive_cutover_advice() {
        let raw = lillux::canonical_json(&serde_json::json!({
            "schema_version": LAUNCH_METADATA_SCHEMA_VERSION + 1,
            "resume_context": {"deliberately": "newer"}
        }))
        .unwrap();

        let error = decode_current_launch_metadata(&raw).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("not the exact current contract"));
        assert!(message.contains(&format!(
            "stored schema_version={}",
            LAUNCH_METADATA_SCHEMA_VERSION + 1
        )));
        assert!(
            !message.contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND)
        );
        assert!(!requires_execution_schema_cutover(&error));
        assert!(is_newer_execution_schema(&error));
    }

    #[test]
    fn workspace_states_have_one_strict_canonical_persistence_spelling() {
        for state in WorkspaceState::ALL {
            let encoded = state.as_str();
            assert_eq!(encoded.parse::<WorkspaceState>().unwrap(), state);
            assert_eq!(state.to_string(), encoded);
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{encoded}\"")
            );
        }

        assert!("Closing".parse::<WorkspaceState>().is_err());
        assert!("unknown".parse::<WorkspaceState>().is_err());
    }

    #[test]
    fn workspace_state_sql_contract_matches_the_rust_enum() {
        let (_tmp, db) = fresh_db();
        for (index, state) in WorkspaceState::ALL.into_iter().enumerate() {
            let workspace_id = format!("workspace-{index}");
            db.conn
                .execute(
                    "INSERT INTO execution_workspace
                         (workspace_id, lower_snapshot, root_path, state,
                          created_at_ms, updated_at_ms)
                     VALUES (?1, 'snapshot', ?2, ?3, 1, 1)",
                    params![
                        workspace_id,
                        format!("/tmp/workspace-{index}"),
                        state.as_str()
                    ],
                )
                .unwrap();
            let persisted: WorkspaceState = db
                .conn
                .query_row(
                    "SELECT state FROM execution_workspace WHERE workspace_id=?1",
                    [&workspace_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(persisted, state);
        }

        assert!(
            db.conn
                .execute(
                    "INSERT INTO execution_workspace
                     (workspace_id, lower_snapshot, root_path, state,
                      created_at_ms, updated_at_ms)
                 VALUES ('workspace-invalid', 'snapshot', '/tmp/workspace-invalid',
                         'invalid', 1, 1)",
                    [],
                )
                .is_err()
        );
    }

    fn rewrite_as_recognized_runtime_predecessor(
        db: &RuntimeDb,
        phase: &str,
        parent_successor_thread_id: Option<&str>,
    ) {
        db.conn
            .execute_batch(
                &format!(
                    "PRAGMA application_id = {PREDECESSOR_RUNTIME_APP_ID};
                 DROP INDEX idx_hook_dispatch_ledger_chain_root;
                 DROP TABLE hook_dispatch_ledger;
                 DROP INDEX idx_follow_waiter_successor;
                 DROP INDEX idx_follow_waiter_child_chain2;
                 DROP TABLE follow_waiter_child;
                 DROP TABLE follow_waiter;

                 CREATE TABLE follow_waiter (
                     follow_key TEXT PRIMARY KEY,
                     parent_thread_id TEXT NOT NULL,
                     parent_chain_root_id TEXT NOT NULL,
                     parent_successor_thread_id TEXT,
                     follow_node TEXT NOT NULL,
                     graph_run_id TEXT NOT NULL,
                     step_count INTEGER NOT NULL,
                     frontier_id TEXT,
                     child_thread_id TEXT,
                     child_chain_root_id TEXT,
                     child_terminal_thread_id TEXT,
                     child_terminal_status TEXT,
                     terminal_envelope TEXT,
                     phase TEXT NOT NULL CHECK (phase IN ('reserved', 'waiting', 'ready', 'resuming')),
                     created_at_ms INTEGER NOT NULL,
                     updated_at_ms INTEGER NOT NULL,
                     fanout INTEGER NOT NULL DEFAULT 0,
                     expected_children INTEGER NOT NULL DEFAULT 1
                 );
                 CREATE UNIQUE INDEX idx_follow_waiter_successor
                     ON follow_waiter(parent_successor_thread_id);
                 CREATE UNIQUE INDEX idx_follow_waiter_child_chain
                     ON follow_waiter(child_chain_root_id);

                 CREATE TABLE follow_waiter_child (
                     follow_key TEXT NOT NULL,
                     item_index INTEGER NOT NULL,
                     item_ref TEXT NOT NULL,
                     spec_hash TEXT NOT NULL,
                     child_thread_id TEXT NOT NULL,
                     child_chain_root_id TEXT NOT NULL,
                     terminal_thread_id TEXT,
                     terminal_status TEXT,
                     terminal_envelope TEXT,
                     created_at_ms INTEGER NOT NULL,
                     updated_at_ms INTEGER NOT NULL,
                     PRIMARY KEY (follow_key, item_index)
                 );
                 CREATE UNIQUE INDEX idx_follow_waiter_child_chain2
                     ON follow_waiter_child(child_chain_root_id);"
                ),
            )
            .unwrap();

        db.conn
            .execute(
                "INSERT INTO thread_runtime
                    (thread_id,chain_root_id,launch_metadata)
                 VALUES ('T-legacy-child','T-legacy-child',?1)",
                [serde_json::json!({"schema_version": 2}).to_string()],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO follow_waiter
                    (follow_key,parent_thread_id,parent_chain_root_id,
                     parent_successor_thread_id,follow_node,graph_run_id,step_count,
                     child_thread_id,child_chain_root_id,phase,created_at_ms,updated_at_ms)
                 VALUES ('follow-old','T-parent','T-parent',?1,'fan','run-old',3,
                         'T-legacy-child','T-legacy-child',?2,10,11)",
                params![parent_successor_thread_id, phase],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO follow_waiter_child
                    (follow_key,item_index,item_ref,spec_hash,child_thread_id,
                     child_chain_root_id,created_at_ms,updated_at_ms)
                 VALUES ('follow-old',0,'tool:test/old','spec-old','T-legacy-child',
                         'T-legacy-child',10,11)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO thread_child_link
                    (child_thread_id,parent_thread_id,relation,created_at_ms)
                 VALUES ('T-legacy-child','T-parent','follow',10)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO launch_window
                    (child_chain_root_id,window_key,width,created_at_ms)
                 VALUES ('T-legacy-child','follow:old',1,10)",
                [],
            )
            .unwrap();
    }

    fn hook_seed() -> NewHookDispatch {
        NewHookDispatch {
            seed_version: HOOK_DISPATCH_SEED_VERSION,
            dispatch_key: "a".repeat(64),
            chain_root_id: "T-root".into(),
            caller_thread_id: "T-caller".into(),
            event: "graph_step_completed".into(),
            hook_id: "hook:system/audit".into(),
            request_hash: "b".repeat(64),
        }
    }

    fn hook_response() -> Value {
        serde_json::json!({
            "thread": {"id": "T-hook", "status": "completed"},
            "result": {"accepted": true, "cost": 7}
        })
    }

    #[test]
    fn hook_dispatch_reserve_pending_complete_and_replay() {
        let (_tmp, db) = fresh_db();
        let seed = hook_seed();
        assert!(matches!(
            db.reserve_hook_dispatch(&seed).unwrap(),
            HookDispatchReservation::Execute
        ));
        assert!(matches!(
            db.reserve_hook_dispatch(&seed).unwrap(),
            HookDispatchReservation::PendingUnknown
        ));

        let response = hook_response();
        db.complete_hook_dispatch(&seed.dispatch_key, &seed.request_hash, &response)
            .unwrap();
        let HookDispatchReservation::Replay(replayed) = db.reserve_hook_dispatch(&seed).unwrap()
        else {
            panic!("completed hook dispatch must replay");
        };
        assert_eq!(replayed.response, response);
        db.complete_hook_dispatch(&seed.dispatch_key, &seed.request_hash, &response)
            .unwrap();
    }

    #[test]
    fn hook_dispatch_reservation_rejects_a_non_current_seed_version() {
        let (_tmp, db) = fresh_db();
        let mut seed = hook_seed();
        seed.seed_version = HOOK_DISPATCH_SEED_VERSION - 1;
        let error = db.reserve_hook_dispatch(&seed).unwrap_err();
        assert!(error.to_string().contains("not the active version"));
    }

    #[test]
    fn detached_spawn_intent_reuses_one_child_and_rejects_request_drift() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let operation_id = "d".repeat(64);
        let request_hash = "e".repeat(64);
        assert_eq!(
            db.reserve_detached_spawn_intent(
                &operation_id,
                "T-parent",
                &request_hash,
                "T-child-first",
                None,
            )
            .unwrap(),
            "T-child-first"
        );
        assert_eq!(
            db.reserve_detached_spawn_intent(
                &operation_id,
                "T-parent",
                &request_hash,
                "T-child-retry",
                None,
            )
            .unwrap(),
            "T-child-first"
        );
        assert!(
            db.reserve_detached_spawn_intent(
                &operation_id,
                "T-parent",
                &"f".repeat(64),
                "T-child-retry",
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn hook_dispatch_pending_survives_restart_and_completed_replays_to_successor() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let seed = hook_seed();
        {
            let db = RuntimeDb::open(&path).unwrap();
            assert!(matches!(
                db.reserve_hook_dispatch(&seed).unwrap(),
                HookDispatchReservation::Execute
            ));
        }
        let db = RuntimeDb::open(&path).unwrap();
        assert!(matches!(
            db.reserve_hook_dispatch(&seed).unwrap(),
            HookDispatchReservation::PendingUnknown
        ));
        let response = hook_response();
        db.complete_hook_dispatch(&seed.dispatch_key, &seed.request_hash, &response)
            .unwrap();

        let mut successor = seed;
        successor.caller_thread_id = "T-successor".to_string();
        let HookDispatchReservation::Replay(replayed) =
            db.reserve_hook_dispatch(&successor).unwrap()
        else {
            panic!("successor segment must replay the chain-scoped response");
        };
        assert_eq!(replayed.response, response);
    }

    #[test]
    fn hook_dispatch_crash_boundaries_never_invoke_twice() {
        let (_tmp, db) = fresh_db();

        // Reservation committed, then the handler disappears before dispatch.
        let before_dispatch = hook_seed();
        let mut invocations = 0;
        assert!(matches!(
            db.reserve_hook_dispatch(&before_dispatch).unwrap(),
            HookDispatchReservation::Execute
        ));
        assert!(matches!(
            db.reserve_hook_dispatch(&before_dispatch).unwrap(),
            HookDispatchReservation::PendingUnknown
        ));
        assert_eq!(invocations, 0);

        // The child returns, but completion is lost: redrive observes pending
        // and must not invoke the child again.
        let mut before_completion = hook_seed();
        before_completion.dispatch_key = "c".repeat(64);
        assert!(matches!(
            db.reserve_hook_dispatch(&before_completion).unwrap(),
            HookDispatchReservation::Execute
        ));
        invocations += 1;
        assert!(matches!(
            db.reserve_hook_dispatch(&before_completion).unwrap(),
            HookDispatchReservation::PendingUnknown
        ));
        assert_eq!(invocations, 1);

        // Completion commits but the response is lost in transport: redrive
        // returns the exact response, including the cost-bearing leaf result,
        // without a second invocation.
        let mut after_completion = hook_seed();
        after_completion.dispatch_key = "d".repeat(64);
        assert!(matches!(
            db.reserve_hook_dispatch(&after_completion).unwrap(),
            HookDispatchReservation::Execute
        ));
        invocations += 1;
        let response = hook_response();
        db.complete_hook_dispatch(
            &after_completion.dispatch_key,
            &after_completion.request_hash,
            &response,
        )
        .unwrap();
        let HookDispatchReservation::Replay(replayed) =
            db.reserve_hook_dispatch(&after_completion).unwrap()
        else {
            panic!("completed dispatch must replay after response loss");
        };
        assert_eq!(replayed.response, response);
        assert_eq!(invocations, 2);
    }

    #[test]
    fn hook_dispatch_rejects_identity_and_completion_drift() {
        let (_tmp, db) = fresh_db();
        let seed = hook_seed();
        db.reserve_hook_dispatch(&seed).unwrap();

        let mut conflicting_seed = seed.clone();
        conflicting_seed.request_hash = "c".repeat(64);
        assert!(db.reserve_hook_dispatch(&conflicting_seed).is_err());

        let response = hook_response();
        db.complete_hook_dispatch(&seed.dispatch_key, &seed.request_hash, &response)
            .unwrap();
        let divergent = serde_json::json!({
            "thread": {"id": "T-hook", "status": "completed"},
            "result": {"accepted": false}
        });
        assert!(
            db.complete_hook_dispatch(&seed.dispatch_key, &seed.request_hash, &divergent)
                .is_err()
        );
    }

    #[test]
    fn hook_dispatch_rejects_invalid_oversize_and_corrupt_responses() {
        let (_tmp, db) = fresh_db();
        let seed = hook_seed();
        db.reserve_hook_dispatch(&seed).unwrap();
        assert!(
            db.complete_hook_dispatch(
                &seed.dispatch_key,
                &seed.request_hash,
                &serde_json::json!({"result": {}}),
            )
            .is_err()
        );
        let oversize = serde_json::json!({
            "thread": {},
            "result": {"body": "x".repeat(MAX_HOOK_DISPATCH_RESPONSE_BYTES)}
        });
        assert!(
            db.complete_hook_dispatch(&seed.dispatch_key, &seed.request_hash, &oversize)
                .is_err()
        );

        let response = hook_response();
        db.complete_hook_dispatch(&seed.dispatch_key, &seed.request_hash, &response)
            .unwrap();
        db.conn
            .execute(
                "UPDATE hook_dispatch_ledger SET response_json=?2 WHERE dispatch_key=?1",
                params![seed.dispatch_key, b"{}".as_slice()],
            )
            .unwrap();
        assert!(db.reserve_hook_dispatch(&seed).is_err());
    }

    #[test]
    fn deleting_chain_runtime_removes_hook_ledger_without_making_it_live() {
        let (_tmp, db) = fresh_db();
        let seed = hook_seed();
        db.reserve_hook_dispatch(&seed).unwrap();
        assert!(!db.chain_has_live_state(&seed.chain_root_id).unwrap());
        assert_eq!(
            db.delete_chain_runtime(&seed.chain_root_id, &[]).unwrap(),
            1
        );
        let count: i64 = db
            .conn
            .query_row("SELECT count(*) FROM hook_dispatch_ledger", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    fn fake_process_identity(pid: i64, pgid: i64) -> ExecutionProcessIdentity {
        ExecutionProcessIdentity {
            schema_version: PROCESS_IDENTITY_SCHEMA_VERSION,
            boot_id: "test-boot".to_string(),
            target_pid: pid,
            target_start_time_ticks: 10,
            group_leader_pid: pgid,
            group_leader_start_time_ticks: 20,
        }
    }

    #[test]
    fn attach_and_read_launch_metadata_roundtrip() {
        let (_tmp, db) = fresh_db();
        db.insert_thread_runtime("t1", "c1").unwrap();
        let lm = RuntimeLaunchMetadata {
            cancellation_mode: Some(CancellationMode::Graceful { grace_secs: 9 }),
            ..Default::default()
        };
        db.attach_process("t1", 1234, 5678, &fake_process_identity(1234, 5678), &lm)
            .unwrap();

        let info = db.get_runtime_info("t1").unwrap().unwrap();
        assert_eq!(info.pid, Some(1234));
        assert_eq!(info.pgid, Some(5678));
        let back = info.launch_metadata.expect("launch_metadata");
        assert_eq!(back.cancellation_mode, lm.cancellation_mode);
    }

    #[test]
    fn child_links_walk_transitively_in_spawn_order() {
        let (_tmp, db) = fresh_db();
        // parent → a, b ; a → a1 ; a1 → a2 (a chain under one branch).
        db.record_child_link("parent", "a", "inline").unwrap();
        db.record_child_link("parent", "b", "follow").unwrap();
        db.record_child_link("a", "a1", "inline").unwrap();
        db.record_child_link("a1", "a2", "inline").unwrap();

        let descendants = db.descendant_thread_ids("parent").unwrap();
        assert_eq!(descendants, vec!["a", "b", "a1", "a2"]);

        // A subtree root walks only its own descendants.
        assert_eq!(db.descendant_thread_ids("a").unwrap(), vec!["a1", "a2"]);
        // A leaf has none.
        assert!(db.descendant_thread_ids("a2").unwrap().is_empty());
    }

    #[test]
    fn record_child_link_is_idempotent_on_the_child() {
        let (_tmp, db) = fresh_db();
        assert_eq!(
            db.record_child_link("parent", "child", "inline").unwrap(),
            ChildLinkInsertOutcome::Inserted
        );
        // A re-driven launch of the same child must not error or duplicate.
        assert_eq!(
            db.record_child_link("parent", "child", "inline").unwrap(),
            ChildLinkInsertOutcome::AlreadyPresent
        );
        assert_eq!(db.descendant_thread_ids("parent").unwrap(), vec!["child"]);
    }

    #[test]
    fn record_child_link_rejects_conflicting_parent_or_relation() {
        let (_tmp, db) = fresh_db();
        db.record_child_link("parent", "child", "dispatch").unwrap();

        for (parent, relation) in [("other", "dispatch"), ("parent", "continuation")] {
            let error = db
                .record_child_link(parent, "child", relation)
                .expect_err("conflicting child authority must fail");
            assert!(error.to_string().contains("refusing conflicting"));
        }
        assert_eq!(db.descendant_thread_ids("parent").unwrap(), vec!["child"]);
        assert!(db.descendant_thread_ids("other").unwrap().is_empty());
    }

    #[test]
    fn child_link_and_new_child_stop_commit_together() {
        let (_tmp, db) = fresh_db();
        db.insert_thread_runtime("child", "child").unwrap();

        let (outcome, stop) = db
            .record_child_link_with_stop_policy(
                "parent",
                "child",
                "dispatch",
                ChildLinkStopPolicy::IfInserted(StopIntent::Cancel),
            )
            .unwrap();
        assert_eq!(outcome, ChildLinkInsertOutcome::Inserted);
        assert_eq!(stop, Some(StopIntent::Cancel));
        assert_eq!(
            db.get_runtime_info("child").unwrap().unwrap().stop_intent,
            Some(StopIntent::Cancel)
        );

        let (outcome, stop) = db
            .record_child_link_with_stop_policy(
                "parent",
                "child",
                "dispatch",
                ChildLinkStopPolicy::IfInserted(StopIntent::Kill),
            )
            .unwrap();
        assert_eq!(outcome, ChildLinkInsertOutcome::AlreadyPresent);
        assert_eq!(stop, None);
    }

    #[test]
    fn child_link_rolls_back_when_atomic_stop_cannot_be_written() {
        let (_tmp, db) = fresh_db();
        let error = db
            .record_child_link_with_stop_policy(
                "parent",
                "missing-child-runtime",
                "dispatch",
                ChildLinkStopPolicy::IfInserted(StopIntent::Cancel),
            )
            .expect_err("missing stop target must roll back lineage");
        assert!(error.to_string().contains("thread_runtime row missing"));
        assert!(db.descendant_thread_ids("parent").unwrap().is_empty());
    }

    #[test]
    fn conflicting_child_link_is_rejected_before_stop_policy() {
        let (_tmp, db) = fresh_db();
        db.insert_thread_runtime("child", "child").unwrap();
        db.record_child_link("parent", "child", "dispatch").unwrap();

        db.record_child_link_with_stop_policy(
            "other",
            "child",
            "dispatch",
            ChildLinkStopPolicy::Always(StopIntent::Kill),
        )
        .expect_err("conflicting authority must fail before tombstoning child");
        assert_eq!(
            db.get_runtime_info("child").unwrap().unwrap().stop_intent,
            None
        );
    }

    #[test]
    fn descendant_walk_terminates_on_a_link_cycle() {
        let (_tmp, db) = fresh_db();
        // A pathological cycle (a → b → a) must not drive an unbounded walk.
        // From `a`, the only descendant is `b`; the back-edge to `a` is dropped
        // because the root is pre-seeded into the `seen` set.
        db.record_child_link("a", "b", "inline").unwrap();
        db.record_child_link("b", "a", "inline").unwrap();
        assert_eq!(db.descendant_thread_ids("a").unwrap(), vec!["b"]);
    }

    #[test]
    fn settle_open_commands_completes_fulfilled_rejects_the_rest_for_the_thread_only() {
        let (_tmp, db) = fresh_db();
        let mk = |thread: &str, kind: &str| NewCommandRecord {
            thread_id: thread.to_string(),
            command_type: kind.to_string(),
            requested_by: None,
            params: None,
        };
        let cancel = db.submit_command(&mk("t1", "cancel")).unwrap();
        let kill = db.submit_command(&mk("t1", "kill")).unwrap();
        let other = db.submit_command(&mk("t2", "cancel")).unwrap();
        // Claim t1's commands so one open command is `claimed`, the other `pending`.
        db.claim_commands(
            "t1",
            MAX_COMMAND_CLAIM_ITEMS,
            MAX_COMMAND_CLAIM_RESPONSE_BYTES,
        )
        .unwrap();

        // Thread finalized `cancelled`: the cancel command was fulfilled, the kill
        // was not.
        let settled = db.settle_open_commands("t1", "cancelled").unwrap();
        assert_eq!(settled.len(), 2, "both open commands settled");
        assert!(
            settled
                .iter()
                .all(|r| r.completed_at.is_some() && r.result.is_some())
        );
        assert_eq!(
            db.get_command(cancel.command_id).unwrap().unwrap().status,
            "completed",
            "cancel fulfilled by a cancelled terminal"
        );
        assert_eq!(
            db.get_command(kill.command_id).unwrap().unwrap().status,
            "rejected",
            "kill not fulfilled by a cancelled terminal"
        );
        // Another thread's command is untouched.
        assert_eq!(
            db.get_command(other.command_id).unwrap().unwrap().status,
            "pending"
        );
        // Idempotent: nothing open remains to settle.
        assert!(
            db.settle_open_commands("t1", "cancelled")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn command_payload_limits_reject_before_durable_transition() {
        let (_tmp, db) = fresh_db();
        let oversized = Value::String("x".repeat(MAX_COMMAND_PARAMS_BYTES));
        let oversized_submit = NewCommandRecord {
            thread_id: "t1".to_string(),
            command_type: "cancel".to_string(),
            requested_by: None,
            params: Some(oversized.clone()),
        };
        assert!(db.submit_command(&oversized_submit).is_err());
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM thread_commands", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "oversized params must not create a command");

        let command = db
            .submit_command(&NewCommandRecord {
                thread_id: "t1".to_string(),
                command_type: "cancel".to_string(),
                requested_by: None,
                params: None,
            })
            .unwrap();
        assert!(
            db.complete_command(command.command_id, "completed", Some(&oversized))
                .is_err()
        );
        assert_eq!(
            db.get_command(command.command_id).unwrap().unwrap().status,
            "pending",
            "oversized result must not settle the command"
        );
    }

    #[test]
    fn command_type_policy_is_enforced_at_the_durable_boundary() {
        let (_tmp, db) = fresh_db();
        for command_type in ["cancel", "kill", "interrupt", "continue"] {
            db.submit_command(&NewCommandRecord {
                thread_id: format!("valid-{command_type}"),
                command_type: command_type.to_string(),
                requested_by: None,
                params: None,
            })
            .unwrap();
        }

        for command_type in ["", "pause", "Cancel", "continue "] {
            assert!(
                db.submit_command(&NewCommandRecord {
                    thread_id: "invalid-command".to_string(),
                    command_type: command_type.to_string(),
                    requested_by: None,
                    params: None,
                })
                .is_err()
            );
        }
        let invalid_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM thread_commands WHERE thread_id = 'invalid-command'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(invalid_count, 0);
    }

    #[test]
    fn settlement_result_limit_is_checked_before_any_command_is_updated() {
        let (_tmp, db) = fresh_db();
        let first = db
            .submit_command(&NewCommandRecord {
                thread_id: "settlement-bounds".to_string(),
                command_type: "cancel".to_string(),
                requested_by: None,
                params: None,
            })
            .unwrap();
        let second = db
            .submit_command(&NewCommandRecord {
                thread_id: "settlement-bounds".to_string(),
                command_type: "kill".to_string(),
                requested_by: None,
                params: None,
            })
            .unwrap();

        let oversized_terminal_status = "x".repeat(MAX_COMMAND_RESULT_BYTES);
        assert!(
            db.settle_open_commands("settlement-bounds", &oversized_terminal_status)
                .is_err()
        );
        for command_id in [first.command_id, second.command_id] {
            let command = db.get_command(command_id).unwrap().unwrap();
            assert_eq!(command.status, "pending");
            assert!(command.result.is_none());
            assert!(command.completed_at.is_none());
        }
    }

    #[test]
    fn command_claim_limits_leave_unreturned_commands_pending() {
        let (_tmp, db) = fresh_db();
        let new_command = || NewCommandRecord {
            thread_id: "t1".to_string(),
            command_type: "cancel".to_string(),
            requested_by: None,
            params: None,
        };
        let first = db.submit_command(&new_command()).unwrap();
        let second = db.submit_command(&new_command()).unwrap();
        let third = db.submit_command(&new_command()).unwrap();

        let claimed = db
            .claim_commands("t1", 2, MAX_COMMAND_CLAIM_RESPONSE_BYTES)
            .unwrap();
        assert_eq!(
            claimed
                .iter()
                .map(|command| command.command_id)
                .collect::<Vec<_>>(),
            vec![first.command_id, second.command_id]
        );
        assert_eq!(
            db.get_command(third.command_id).unwrap().unwrap().status,
            "pending"
        );
        assert_eq!(
            db.claim_commands("t1", 2, MAX_COMMAND_CLAIM_RESPONSE_BYTES)
                .unwrap()[0]
                .command_id,
            third.command_id
        );

        let tiny_budget_command = db
            .submit_command(&NewCommandRecord {
                thread_id: "t2".to_string(),
                ..new_command()
            })
            .unwrap();
        assert!(db.claim_commands("t2", 1, 32).is_err());
        assert_eq!(
            db.get_command(tiny_budget_command.command_id)
                .unwrap()
                .unwrap()
                .status,
            "pending",
            "a response-budget failure must not claim the command"
        );
    }

    #[test]
    fn open_command_quota_rejects_without_mutation_and_bounds_settlement() {
        let (_tmp, db) = fresh_db();
        for _ in 0..MAX_OPEN_COMMANDS_PER_THREAD {
            db.submit_command(&NewCommandRecord {
                thread_id: "bounded-thread".to_string(),
                command_type: "cancel".to_string(),
                requested_by: None,
                params: None,
            })
            .unwrap();
        }
        assert!(
            db.submit_command(&NewCommandRecord {
                thread_id: "bounded-thread".to_string(),
                command_type: "cancel".to_string(),
                requested_by: None,
                params: None,
            })
            .is_err()
        );
        let open_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM thread_commands \
                 WHERE thread_id = 'bounded-thread' AND status IN ('pending', 'claimed')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(open_count as usize, MAX_OPEN_COMMANDS_PER_THREAD);
        assert_eq!(
            db.settle_open_commands("bounded-thread", "failed")
                .unwrap()
                .len(),
            MAX_OPEN_COMMANDS_PER_THREAD
        );
    }

    #[test]
    fn thread_has_kill_command_detects_the_kill_intent_marker() {
        let (_tmp, db) = fresh_db();
        let mk = |thread: &str, kind: &str| NewCommandRecord {
            thread_id: thread.to_string(),
            command_type: kind.to_string(),
            requested_by: None,
            params: None,
        };
        db.submit_command(&mk("t1", "cancel")).unwrap();
        assert!(!db.thread_has_kill_command("t1").unwrap());
        db.submit_command(&mk("t1", "kill")).unwrap();
        assert!(db.thread_has_kill_command("t1").unwrap());
        // Scoped to the thread.
        assert!(!db.thread_has_kill_command("t2").unwrap());
    }

    #[test]
    fn empty_attach_preserves_seeded_launch_metadata() {
        // Spawn seeds real metadata; a later UDS self-attach sends only pid/pgid
        // (empty metadata) and must NOT clobber it.
        let (_tmp, db) = fresh_db();
        db.insert_thread_runtime("t1", "c1").unwrap();
        let seeded = RuntimeLaunchMetadata {
            cancellation_mode: Some(CancellationMode::Graceful { grace_secs: 9 }),
            ..Default::default()
        };
        db.attach_process(
            "t1",
            1234,
            5678,
            &fake_process_identity(1234, 5678),
            &seeded,
        )
        .unwrap();

        // Exact self-attach with default (empty) metadata is idempotent.
        db.attach_process(
            "t1",
            1234,
            5678,
            &fake_process_identity(1234, 5678),
            &RuntimeLaunchMetadata::default(),
        )
        .unwrap();

        let info = db.get_runtime_info("t1").unwrap().unwrap();
        assert_eq!(info.pid, Some(1234));
        assert_eq!(info.pgid, Some(5678));
        assert_eq!(
            info.launch_metadata
                .expect("seeded metadata preserved")
                .cancellation_mode,
            seeded.cancellation_mode,
            "empty attach must not clobber seeded metadata"
        );

        let replacement = db
            .attach_process(
                "t1",
                4321,
                8765,
                &fake_process_identity(4321, 8765),
                &RuntimeLaunchMetadata::default(),
            )
            .unwrap_err();
        assert!(format!("{replacement:#}").contains("immutable process identity"));
    }

    #[test]
    fn attach_with_hard_cancellation_roundtrip() {
        let (_tmp, db) = fresh_db();
        db.insert_thread_runtime("t1", "c1").unwrap();
        let lm = RuntimeLaunchMetadata {
            cancellation_mode: Some(CancellationMode::Hard),
            ..Default::default()
        };
        db.attach_process("t1", 101, 102, &fake_process_identity(101, 102), &lm)
            .unwrap();
        let info = db.get_runtime_info("t1").unwrap().unwrap();
        assert_eq!(
            info.launch_metadata.unwrap().cancellation_mode,
            Some(CancellationMode::Hard)
        );
    }

    #[test]
    fn open_is_idempotent() {
        let (tmp, db) = fresh_db();
        let path = tmp.path().join("runtime.db");
        drop(db);
        drop(RuntimeDb::open(&path).unwrap());
        drop(RuntimeDb::open(&path).unwrap());
    }

    #[test]
    fn empty_installed_schema_with_unset_authority_columns_remains_current() {
        let (tmp, db) = fresh_db();
        let path = tmp.path().join("runtime.db");
        for table in ["detached_spawn_intent", "follow_waiter"] {
            let mut statement = db
                .conn
                .prepare(&format!("PRAGMA table_info({table})"))
                .unwrap();
            let columns = statement
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert!(
                columns
                    .iter()
                    .any(|column| column == "child_project_authority")
            );
        }
        drop(db);
        let reopened = RuntimeDb::open(&path).unwrap();
        assert!(!reopened.requires_explicit_history_reset());
    }

    #[test]
    fn launch_window_admits_to_width_then_queues_fifo() {
        let (_tmp, db) = fresh_db();
        db.launch_window_insert("c1", "P:gr:fan", 2, 1).unwrap();
        assert_eq!(
            db.launch_window_admit("P:gr:fan", None, 1).unwrap(),
            vec!["c1"]
        );
        db.launch_window_insert("c2", "P:gr:fan", 2, 2).unwrap();
        assert_eq!(
            db.launch_window_admit("P:gr:fan", None, 2).unwrap(),
            vec!["c2"]
        );
        // Width 2 reached — the third member queues.
        db.launch_window_insert("c3", "P:gr:fan", 2, 3).unwrap();
        assert!(
            db.launch_window_admit("P:gr:fan", None, 3)
                .unwrap()
                .is_empty()
        );
        assert!(db.launch_window_is_queued("c3").unwrap());
        assert!(db.launch_window_is_member("c3").unwrap());
        assert!(!db.launch_window_is_queued("c1").unwrap());

        // A hard terminal releases the slot and admits the oldest queued.
        assert_eq!(db.launch_window_release("c1", None, 4).unwrap(), vec!["c3"]);
        assert!(!db.launch_window_is_member("c1").unwrap());
        assert!(!db.launch_window_is_queued("c3").unwrap());

        // Releasing a non-member is a no-op.
        assert!(
            db.launch_window_release("nope", None, 5)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn launch_window_global_ceiling_caps_across_windows() {
        let (_tmp, db) = fresh_db();
        db.launch_window_insert("a1", "P:one", 5, 1).unwrap();
        db.launch_window_insert("b1", "Q:two", 5, 2).unwrap();
        // Global ceiling of 1: only the first window admits.
        assert_eq!(
            db.launch_window_admit("P:one", Some(1), 3).unwrap(),
            vec!["a1"]
        );
        assert!(
            db.launch_window_admit("Q:two", Some(1), 4)
                .unwrap()
                .is_empty()
        );
        // Release wakes the globally oldest eligible row immediately; a
        // different window never waits for the maintenance sweep.
        assert_eq!(
            db.launch_window_release("a1", Some(1), 5).unwrap(),
            vec!["b1"]
        );
        assert_eq!(
            db.launch_window_keys_with_queue().unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn suspended_window_member_yields_global_slot_to_nested_follow() {
        let (_tmp, db) = fresh_db();
        db.launch_window_insert("chain-parent", "outer", 1, 1)
            .unwrap();
        assert_eq!(
            db.launch_window_admit("outer", Some(1), 2).unwrap(),
            vec!["chain-parent"]
        );

        let follow_key = "nested-follow";
        db.reserve_follow(&seed_follow(follow_key)).unwrap();
        set_single_follow_child(&db, follow_key, "nested-thread", "nested-chain").unwrap();
        db.set_follow_parent_successor(follow_key, "parent-successor")
            .unwrap();
        assert_eq!(db.mark_follow_waiting(follow_key).unwrap(), "waiting");

        let nested_window = format!("follow:{follow_key}");
        db.launch_window_insert("nested-chain", &nested_window, 1, 3)
            .unwrap();
        assert_eq!(
            db.launch_window_admit(&nested_window, Some(1), 4).unwrap(),
            vec!["nested-chain"],
            "a suspended parent must not consume the only node execution slot"
        );
        assert_eq!(db.launch_window_live_total().unwrap(), 1);
    }

    #[test]
    fn cap_one_nested_completion_releases_child_before_parent_resume() {
        let (_tmp, db) = fresh_db();
        db.launch_window_insert("chain-parent", "outer", 1, 1)
            .unwrap();
        assert_eq!(
            db.launch_window_admit_global(Some(1), 2).unwrap(),
            vec!["chain-parent"]
        );

        let follow_key = "nested-follow-completion";
        db.reserve_follow(&seed_follow(follow_key)).unwrap();
        set_single_follow_child(&db, follow_key, "nested-thread", "nested-chain").unwrap();
        db.set_follow_parent_successor(follow_key, "parent-successor")
            .unwrap();
        db.mark_follow_waiting(follow_key).unwrap();
        db.launch_window_insert("nested-chain", &format!("follow:{follow_key}"), 1, 3)
            .unwrap();
        assert_eq!(
            db.launch_window_admit_global(Some(1), 4).unwrap(),
            vec!["nested-chain"]
        );

        assert!(
            db.mark_follow_child_terminal(
                "nested-chain",
                "nested-tail",
                "completed",
                &serde_json::json!({"status":"completed","result":{}}),
            )
            .unwrap()
        );
        assert_eq!(
            db.launch_window_live_total().unwrap(),
            2,
            "READY parent counts again until the terminal child releases its slot"
        );
        assert!(
            db.launch_window_release("nested-chain", Some(1), 5)
                .unwrap()
                .is_empty()
        );
        assert_eq!(db.launch_window_live_total().unwrap(), 1);
        assert!(!db.launch_window_is_member("nested-chain").unwrap());
    }

    #[test]
    fn launch_window_sweep_inputs_expose_launched_and_queued() {
        let (_tmp, db) = fresh_db();
        db.launch_window_insert("c1", "K", 1, 1).unwrap();
        db.launch_window_insert("c2", "K", 1, 2).unwrap();
        db.launch_window_admit("K", None, 3).unwrap();
        assert_eq!(db.launch_window_launched_members().unwrap(), vec!["c1"]);
        assert_eq!(db.launch_window_keys_with_queue().unwrap(), vec!["K"]);
    }

    #[test]
    fn cancellation_tombstones_queued_and_admitted_members_without_replacement() {
        let (_tmp, mut db) = fresh_db();
        db.launch_window_insert("admitted", "K", 1, 1).unwrap();
        db.launch_window_insert("queued", "K", 1, 2).unwrap();
        assert_eq!(
            db.launch_window_admit("K", None, 3).unwrap(),
            vec!["admitted"]
        );
        assert_eq!(
            db.launch_window_cancel_members(&["queued".into(), "admitted".into()], 4)
                .unwrap(),
            vec!["queued", "admitted"]
        );
        assert!(db.launch_window_admit("K", None, 5).unwrap().is_empty());
        assert_eq!(
            db.launch_window_cancelled_members().unwrap(),
            vec!["admitted", "queued"]
        );
        db.launch_window_discard_member("admitted").unwrap();
        db.launch_window_discard_member("queued").unwrap();
        assert!(db.launch_window_cancelled_members().unwrap().is_empty());
    }

    #[test]
    fn discarded_cancelled_member_wakes_oldest_global_replacement() {
        let (_tmp, mut db) = fresh_db();
        db.launch_window_insert("cancelled", "A", 1, 1).unwrap();
        db.launch_window_insert("replacement", "B", 1, 2).unwrap();
        assert_eq!(
            db.launch_window_admit_global(Some(1), 3).unwrap(),
            vec!["cancelled"]
        );
        assert_eq!(
            db.launch_window_cancel_members(&["cancelled".into()], 4)
                .unwrap(),
            vec!["cancelled"]
        );
        db.launch_window_discard_member("cancelled").unwrap();
        assert_eq!(
            db.launch_window_admit_global(Some(1), 5).unwrap(),
            vec!["replacement"]
        );
    }

    /// Unknown owned state must fail without mutation. Normal open never
    /// performs a predecessor migration.
    #[test]
    fn open_rejects_unrecognized_owned_db_without_mutating_it() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");

        // Build an OLD owned schema: thread_runtime + thread_commands and
        // their index, stamped with our app_id, but NO thread_launch_claim.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE thread_runtime (
                    thread_id TEXT PRIMARY KEY,
                    chain_root_id TEXT NOT NULL,
                    pid INTEGER,
                    pgid INTEGER,
                    metadata BLOB,
                    launch_metadata TEXT,
                    resume_attempts INTEGER NOT NULL DEFAULT 0
                );
                CREATE INDEX idx_thread_runtime_chain_root
                    ON thread_runtime(chain_root_id);
                CREATE TABLE thread_commands (
                    command_id INTEGER PRIMARY KEY AUTOINCREMENT,
                    thread_id TEXT NOT NULL,
                    command_type TEXT NOT NULL,
                    status TEXT NOT NULL,
                    requested_by TEXT,
                    params BLOB,
                    result BLOB,
                    created_at TEXT NOT NULL,
                    claimed_at TEXT,
                    completed_at TEXT
                );
                CREATE INDEX idx_thread_commands_thread_status
                    ON thread_commands(thread_id, status);
                "#,
            )
            .unwrap();
            conn.execute_batch(&format!("PRAGMA application_id = {};", RUNTIME_APP_ID))
                .unwrap();
            // Seed a runtime row so rejection cannot be confused with an empty
            // file taking the first-initialization branch.
            conn.execute(
                "INSERT INTO thread_runtime (thread_id, chain_root_id, pid, pgid)
                 VALUES (?1, ?2, ?3, ?4)",
                params!["t-old", "c-old", 101_i64, 101_i64],
            )
            .unwrap();
        }

        let error = RuntimeDb::open(&path)
            .err()
            .expect("owned stale runtime database must be rejected");
        assert!(
            !format!("{error:#}")
                .contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND)
        );
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let added: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='thread_launch_claim'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(added, 0);
    }

    #[test]
    fn open_rejects_predecessor_runtime_shape_without_decoding_rows() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let db = RuntimeDb::open(&path).unwrap();
        rewrite_as_recognized_runtime_predecessor(&db, "reserved", None);
        drop(db);

        let error = RuntimeDb::open(&path)
            .err()
            .expect("non-current runtime structure must fail closed");
        assert!(
            !format!("{error:#}")
                .contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND)
        );
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let child_links: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM thread_child_link WHERE child_thread_id='T-legacy-child'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(child_links, 1);
        let incomplete_follows: i64 = conn
            .query_row("SELECT COUNT(*) FROM follow_waiter", [], |row| row.get(0))
            .unwrap();
        assert_eq!(incomplete_follows, 1);
        let incomplete_windows: i64 = conn
            .query_row("SELECT COUNT(*) FROM launch_window", [], |row| row.get(0))
            .unwrap();
        assert_eq!(incomplete_windows, 1);
        let hook_table: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name='hook_dispatch_ledger'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hook_table, 0, "failed migration must roll back its DDL");
        assert!(path.is_file(), "failed migration must retain the database");
    }

    #[test]
    fn unwrapped_detached_project_authority_requires_reset_without_mutation() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let db = RuntimeDb::open(&path).unwrap();
        let predecessor = lillux::canonical_json(&serde_json::json!({
            "kind": "projectless",
            "environment": { "kind": "none" }
        }))
        .unwrap();
        db.conn
            .execute(
                "INSERT INTO detached_spawn_intent(
                     operation_id, parent_thread_id, request_hash, child_thread_id,
                     child_project_authority, created_at_ms
                 ) VALUES ('op-former', 'T-parent', 'request', 'T-child', ?1, 1)",
                params![predecessor],
            )
            .unwrap();
        drop(db);

        let error = RuntimeDb::open(&path)
            .err()
            .expect("unwrapped detached authority must require explicit reset");
        let message = format!("{error:#}");
        assert!(message.contains("explicit no-backcompat reset"));
        assert!(message.contains("stored project authority is not the exact current contract"));
        assert!(message.contains("stored kind=\"projectless\""));
        assert!(!message.contains("missing field `authority`"));

        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let retained: String = conn
            .query_row(
                "SELECT child_project_authority FROM detached_spawn_intent WHERE operation_id='op-former'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, predecessor);
    }

    #[test]
    fn unwrapped_follow_project_authority_requires_reset_without_mutation() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let db = RuntimeDb::open(&path).unwrap();
        let predecessor = lillux::canonical_json(&serde_json::json!({
            "kind": "projectless",
            "environment": { "kind": "none" }
        }))
        .unwrap();
        db.conn
            .execute(
                "INSERT INTO follow_waiter(
                     follow_key, parent_thread_id, parent_chain_root_id, follow_node,
                     graph_run_id, step_count, phase, created_at_ms, updated_at_ms,
                     child_project_authority
                 ) VALUES ('follow-former', 'T-parent', 'T-parent', 'node',
                           'run', 1, 'reserved', 1, 1, ?1)",
                params![predecessor],
            )
            .unwrap();
        drop(db);

        let error = RuntimeDb::open(&path)
            .err()
            .expect("unwrapped follow authority must require explicit reset");
        let message = format!("{error:#}");
        assert!(message.contains("explicit no-backcompat reset"));
        assert!(message.contains("stored project authority is not the exact current contract"));
        assert!(message.contains("stored kind=\"projectless\""));
        assert!(!message.contains("missing field `authority`"));

        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let retained: String = conn
            .query_row(
                "SELECT child_project_authority FROM follow_waiter WHERE follow_key='follow-former'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, predecessor);
    }

    #[test]
    fn open_preserves_former_launch_authority_as_opaque_thread_scoped_history() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let db = RuntimeDb::open(&path).unwrap();
        let mut predecessor = serde_json::to_value(RuntimeLaunchMetadata::default()).unwrap();
        predecessor["schema_version"] = Value::from(LAUNCH_METADATA_SCHEMA_VERSION - 1);
        predecessor["admitted_project_authority"] = serde_json::json!({
            "kind": "live_project",
            "authority_id": "former-authority",
            "authored_project_identity": "local:/tmp/project",
            "canonical_root": "/tmp/project",
            "live_access": {
                "access": "read_write",
                "authorized_write_namespaces": ["project"],
                "denied_control_paths": [".ai"],
                "symlink_policy": "descriptor_rooted_no_escape"
            },
            "environment": { "kind": "none" },
            "capability_ceiling": [],
            "child_policy": { "kind": "inherit" }
        });
        let predecessor = lillux::canonical_json(&predecessor).unwrap();
        db.conn
            .execute(
                "INSERT INTO thread_runtime (
                     thread_id, chain_root_id, launch_metadata
                 ) VALUES (?1, ?2, ?3)",
                params!["T-pre-isolation", "T-pre-isolation", predecessor],
            )
            .unwrap();
        drop(db);

        let reopened =
            RuntimeDb::open(&path).expect("old launch authority must not block node open");
        let runtime = reopened
            .get_runtime_info("T-pre-isolation")
            .unwrap()
            .unwrap();
        assert!(runtime.launch_metadata.is_none());
        assert_eq!(
            runtime.incompatible_launch_metadata,
            Some(IncompatibleLaunchMetadata {
                schema_version: u64::from(LAUNCH_METADATA_SCHEMA_VERSION - 1),
                admitted_launch_capsule_schema: None,
            })
        );
        drop(reopened);

        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let retained: String = conn
            .query_row(
                "SELECT launch_metadata FROM thread_runtime WHERE thread_id='T-pre-isolation'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, predecessor);
    }

    #[test]
    fn predecessor_capsule_schema_is_classified_before_nested_authority_decode() {
        let raw = lillux::canonical_json(&serde_json::json!({
            "schema_version": LAUNCH_METADATA_SCHEMA_VERSION,
            "admitted_launch_capsule_schema":
                ryeos_state::objects::ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION - 1,
            "sealed_root_request": {"deliberately": "not-current-authority"}
        }))
        .unwrap();
        let StoredLaunchMetadata::Incompatible(incompatible) =
            decode_stored_launch_metadata(&raw).unwrap()
        else {
            panic!("predecessor capsule must remain opaque")
        };
        assert_eq!(
            incompatible.admitted_launch_capsule_schema,
            Some(u64::from(
                ryeos_state::objects::ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION - 1
            ))
        );
    }

    #[test]
    fn committed_predecessor_follow_rejects_without_interpreting_rows() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let db = RuntimeDb::open(&path).unwrap();
        rewrite_as_recognized_runtime_predecessor(&db, "waiting", Some("T-successor"));
        drop(db);

        let error = RuntimeDb::open(&path)
            .err()
            .expect("committed predecessor follow must fail closed");
        assert!(
            !format!("{error:#}")
                .contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND)
        );
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let follows: i64 = conn
            .query_row("SELECT COUNT(*) FROM follow_waiter", [], |row| row.get(0))
            .unwrap();
        assert_eq!(follows, 1);
        let hook_table: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name='hook_dispatch_ledger'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hook_table, 0);
    }

    #[test]
    fn explicit_history_reset_replaces_predecessor_runtime_schema_without_migrating_rows() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let db = RuntimeDb::open(&path).unwrap();
        rewrite_as_recognized_runtime_predecessor(&db, "waiting", Some("T-successor"));
        drop(db);

        let directory = lillux::PinnedDirectory::open(tmp.path())
            .unwrap()
            .expect("temporary directory exists");
        let lock = directory.lock_exclusive().unwrap();
        let mut reset = RuntimeDb::open_for_explicit_history_reset_with_namespace_authority(
            &path, directory, lock,
        )
        .unwrap();

        assert!(reset.requires_explicit_history_reset());
        reset.apply_explicit_history_reset(&path).unwrap();
        assert!(!reset.requires_explicit_history_reset());
        assert_current_runtime_schema(&reset.conn, &path).unwrap();
        assert_eq!(
            reset.discard_all_thread_history(true).unwrap().total_rows(),
            0
        );
        assert!(reset.open_workspaces().unwrap().is_empty());
        drop(reset);
        drop(RuntimeDb::open(&path).unwrap());
        let reopened = RuntimeDb::open_existing_current(&path).unwrap();
        assert_eq!(
            reopened
                .discard_all_thread_history(true)
                .unwrap()
                .total_rows(),
            0
        );
    }

    #[test]
    fn projection_rebuild_runtime_open_requires_existing_current_schema() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("missing-runtime.db");
        assert!(RuntimeDb::open_existing_current(&missing).is_err());
        assert!(!missing.exists());

        let current = tmp.path().join("current-runtime.db");
        drop(RuntimeDb::open(&current).unwrap());
        drop(RuntimeDb::open_existing_current(&current).unwrap());
    }

    #[test]
    fn all_thread_history_discard_clears_every_runtime_table_and_preserves_schema() {
        let (tmp, db) = fresh_db();
        db.conn
            .execute_batch(
                "INSERT INTO thread_commands
                     (thread_id, command_type, status, created_at)
                     VALUES ('T-root', 'cancel', 'pending', '2026-07-15T00:00:00Z');
                 INSERT INTO hook_dispatch_ledger
                     (dispatch_key, seed_version, chain_root_id, caller_thread_id, event, hook_id,
                      request_hash, status, created_at_ms)
                     VALUES ('dispatch-1', 2, 'T-root', 'T-root', 'completed', 'hook-1',
                             'request-1', 'pending', 1);
                 INSERT INTO thread_launch_claim
                     (thread_id, claim_id, claimed_at_ms, lease_expires_at_ms, claimed_by)
                     VALUES ('T-root', 'claim-1', 1, 2, 'test');
                 INSERT INTO thread_launch_epoch (thread_id, last_epoch)
                     VALUES ('T-root', 1);
                 INSERT INTO execution_workspace
                     (workspace_id, thread_id, lower_snapshot, root_path, state,
                      created_at_ms, updated_at_ms)
                     VALUES ('workspace-1', 'T-root', 'snapshot-1', '/tmp/workspace-1',
                             'orphaned', 1, 1);
                 INSERT INTO follow_waiter
                     (follow_key, parent_thread_id, parent_chain_root_id, follow_node,
                      graph_run_id, step_count, phase, created_at_ms, updated_at_ms)
                     VALUES ('follow-1', 'T-root', 'T-root', 'node-1', 'run-1', 1,
                             'waiting', 1, 1);
                 INSERT INTO follow_waiter_child
                     (follow_key, item_index, item_ref, spec_hash, child_thread_id,
                      child_chain_root_id, sealed_root_request, created_at_ms, updated_at_ms)
                     VALUES ('follow-1', 0, 'directive:test/child', 'spec-1', 'T-child',
                             'T-child', '{}', 1, 1);
                 INSERT INTO thread_child_link
                     (child_thread_id, parent_thread_id, relation, created_at_ms)
                     VALUES ('T-child', 'T-root', 'follow', 1);
                 INSERT INTO launch_window
                     (child_chain_root_id, window_key, width, created_at_ms)
                     VALUES ('T-child', 'window-1', 1, 1);
                 INSERT INTO seat_lease
                     (seat_thread_id, owner, surface, client_ref, last_seen_at_ms)
                     VALUES ('T-seat', 'owner', 'terminal', 'client', 1);",
            )
            .unwrap();
        db.reserve_in_process_handler_birth("T-root", "T-root", &in_process_launch_metadata())
            .unwrap();
        db.mark_in_process_handler_birth_running("T-root").unwrap();

        let preview = db.discard_all_thread_history(true).unwrap();
        assert_eq!(preview.in_process_handler_reservations, 1);
        assert_eq!(preview.total_rows(), 12);
        assert_eq!(
            db.discard_all_thread_history(true).unwrap().total_rows(),
            12
        );

        let removed = db.discard_all_thread_history(false).unwrap();
        assert_eq!(removed.in_process_handler_reservations, 1);
        assert_eq!(removed.total_rows(), 12);
        assert_eq!(db.discard_all_thread_history(true).unwrap().total_rows(), 0);
        drop(db);

        let path = tmp.path().join("runtime.db");
        let reopened = RuntimeDb::open_existing_current(&path).unwrap();
        let command_id = reopened
            .conn
            .query_row(
                "INSERT INTO thread_commands
                    (thread_id, command_type, status, created_at)
                 VALUES ('T-new', 'cancel', 'pending', '2026-07-15T00:00:00Z')
                 RETURNING command_id",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(
            command_id, 1,
            "command sequence must restart in the empty store"
        );
    }

    #[test]
    fn projection_rebuild_runtime_open_never_migrates_owned_stale_schema() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE thread_runtime (
                thread_id TEXT PRIMARY KEY,
                chain_root_id TEXT NOT NULL,
                pid INTEGER,
                pgid INTEGER,
                metadata BLOB,
                launch_metadata TEXT,
                resume_attempts INTEGER NOT NULL DEFAULT 0
             );",
        )
        .unwrap();
        conn.execute_batch(&format!("PRAGMA application_id = {};", RUNTIME_APP_ID))
            .unwrap();
        drop(conn);

        assert!(RuntimeDb::open_existing_current(&path).is_err());
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let migrated: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='thread_launch_claim'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(migrated, 0);
    }

    #[test]
    fn null_launch_metadata_yields_none() {
        let (_tmp, db) = fresh_db();
        db.insert_thread_runtime("t1", "c1").unwrap();
        db.attach_process(
            "t1",
            107,
            108,
            &fake_process_identity(107, 108),
            &RuntimeLaunchMetadata::default(),
        )
        .unwrap();
        let info = db.get_runtime_info("t1").unwrap().unwrap();
        assert_eq!(info.pid, Some(107));
        assert_eq!(info.pgid, Some(108));
        assert!(info.launch_metadata.is_none());
    }

    #[test]
    fn garbage_launch_metadata_decodes_to_error() {
        // O5: Schema drift / corruption must surface as a typed error,
        // not silently degrade to None.
        let (_tmp, db) = fresh_db();
        db.insert_thread_runtime("t1", "c1").unwrap();
        db.conn
            .execute(
                "UPDATE thread_runtime SET pid = ?2, pgid = ?3, launch_metadata = ?4
                 WHERE thread_id = ?1",
                params!["t1", 1i64, 2i64, "{not valid json"],
            )
            .unwrap();
        let err = db
            .get_runtime_info("t1")
            .expect_err("garbage launch_metadata must error");
        assert!(
            err.to_string().contains("failed to decode launch_metadata"),
            "expected decode error, got: {err}"
        );
        let err = db
            .inspect_chain_recovery_pins("c1", &["t1".to_string()])
            .expect_err("retention must fail closed on unreadable recovery metadata");
        assert!(err.to_string().contains("failed to decode launch_metadata"));
    }

    #[test]
    fn resume_attempts_default_zero_and_bump_increments() {
        let (_tmp, db) = fresh_db();
        db.insert_thread_runtime("t1", "c1").unwrap();
        assert_eq!(db.get_resume_attempts("t1").unwrap(), 0);
        assert_eq!(db.bump_resume_attempts("t1").unwrap(), 1);
        assert_eq!(db.bump_resume_attempts("t1").unwrap(), 2);
        assert_eq!(db.get_resume_attempts("t1").unwrap(), 2);
    }

    #[test]
    fn continuation_runtime_seed_is_retry_safe_and_conditionally_cleaned() {
        let (_tmp, db) = fresh_db();
        let initial = RuntimeLaunchMetadata::default();
        db.seed_continuation_runtime("T-next", "T-root", &initial)
            .unwrap();

        db.seed_continuation_runtime("T-next", "T-root", &initial)
            .unwrap();

        let replacement = RuntimeLaunchMetadata {
            cancellation_mode: Some(CancellationMode::Hard),
            ..Default::default()
        };
        let error = db
            .seed_continuation_runtime("T-next", "T-root", &replacement)
            .unwrap_err();
        assert!(error.to_string().contains("exact unattached"));
        let stored = db
            .get_runtime_info("T-next")
            .unwrap()
            .unwrap()
            .launch_metadata
            .unwrap();
        assert_eq!(stored.cancellation_mode, None);

        assert!(
            db.remove_seeded_continuation_runtime("T-next", "T-root", &initial)
                .unwrap()
        );
        assert!(db.get_runtime_info("T-next").unwrap().is_none());
        assert!(
            !db.remove_seeded_continuation_runtime("T-next", "T-root", &initial)
                .unwrap()
        );
    }

    #[test]
    fn retention_classifier_does_not_pin_historical_resume_or_checkpoint_residue() {
        let (tmp, db) = fresh_db();
        db.insert_thread_runtime("t1", "c1").unwrap();
        db.bump_resume_attempts("t1").unwrap();
        db.set_launch_metadata(
            "t1",
            &RuntimeLaunchMetadata {
                native_resume: Some(Default::default()),
                checkpoint_dir: Some(tmp.path().join("threads/t1/checkpoints")),
                ..Default::default()
            },
        )
        .unwrap();

        let pins = db
            .inspect_chain_recovery_pins("c1", &["t1".to_string()])
            .unwrap();
        assert!(pins.is_empty());
        assert_eq!(pins.recovery_capable_launch_claims, 0);
        assert_eq!(pins.required_checkpoint_consumers, 0);
    }

    #[test]
    fn retention_pins_runtime_membership_conflicts_and_cleanup_covers_them() {
        let (_tmp, db) = fresh_db();
        db.insert_thread_runtime("root", "chain").unwrap();
        db.insert_thread_runtime("orphan-runtime-member", "chain")
            .unwrap();

        let pins = db
            .inspect_chain_recovery_pins("chain", &["root".to_string()])
            .unwrap();
        assert_eq!(pins.runtime_membership_conflicts, 1);
        assert!(!pins.is_empty());

        db.delete_chain_runtime("chain", &["root".to_string()])
            .unwrap();
        assert!(db.get_runtime_info("root").unwrap().is_none());
        assert!(
            db.get_runtime_info("orphan-runtime-member")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn retention_classifier_requires_an_owner_for_recovery_checkpoint_pin() {
        let (tmp, db) = fresh_db();
        db.insert_thread_runtime("t1", "c1").unwrap();
        db.set_launch_metadata(
            "t1",
            &RuntimeLaunchMetadata {
                native_resume: Some(Default::default()),
                checkpoint_dir: Some(tmp.path().join("threads/t1/checkpoints")),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            db.claim_thread_launch("t1", "claim-1", "daemon:test")
                .unwrap(),
            LaunchClaimOutcome::Claimed
        );

        let pins = db
            .inspect_chain_recovery_pins("c1", &["t1".to_string()])
            .unwrap();
        assert_eq!(pins.launch_claims, 1);
        assert_eq!(pins.recovery_capable_launch_claims, 1);
        assert_eq!(pins.required_checkpoint_consumers, 1);
        assert!(!pins.is_empty());

        assert!(db.release_thread_launch_claim("t1", "claim-1").unwrap());
        assert!(
            db.inspect_chain_recovery_pins("c1", &["t1".to_string()])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn retention_classifier_derives_follow_and_cancellation_owners() {
        let (_tmp, mut db) = fresh_db();
        db.insert_thread_runtime("parent-1", "chain-parent")
            .unwrap();
        db.reserve_follow(&seed_follow("follow-1")).unwrap();
        db.submit_command(&NewCommandRecord {
            thread_id: "parent-1".to_string(),
            command_type: "cancel".to_string(),
            requested_by: None,
            params: None,
        })
        .unwrap();
        db.launch_window_insert("chain-parent", "window", 1, 1)
            .unwrap();
        db.launch_window_cancel_members(&["chain-parent".to_string()], 2)
            .unwrap();

        let pins = db
            .inspect_chain_recovery_pins("chain-parent", &["parent-1".to_string()])
            .unwrap();
        assert_eq!(pins.follow_waiters, 1);
        assert_eq!(pins.required_checkpoint_consumers, 1);
        assert_eq!(pins.pending_commands, 1);
        assert_eq!(pins.launch_windows, 1);
        assert_eq!(pins.cancellation_repairs, 2);
        assert!(!pins.is_empty());
    }

    #[test]
    fn resume_attempts_bump_unknown_thread_errors() {
        let (_tmp, db) = fresh_db();
        let err = db.bump_resume_attempts("missing").unwrap_err();
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn resume_attempts_unknown_thread_reads_zero() {
        let (_tmp, db) = fresh_db();
        assert_eq!(db.get_resume_attempts("nope").unwrap(), 0);
    }

    #[test]
    fn attach_process_unknown_thread_errors() {
        // Strict-update: attach must fail loudly when no row exists,
        // so the runner can kill the live child rather than orphaning it.
        let (_tmp, db) = fresh_db();
        let lm = RuntimeLaunchMetadata::default();
        let err = db
            .attach_process("missing", 101, 102, &fake_process_identity(101, 102), &lm)
            .expect_err("attach on missing row must error");
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn schema_version_mismatch_is_explicit_incompatible_authority() {
        // An unsupported authority contract is not current metadata and is not
        // silently lost: reconciliation receives its explicit outer marker.
        let (_tmp, db) = fresh_db();
        db.insert_thread_runtime("t1", "c1").unwrap();
        let mut payload = serde_json::to_value(RuntimeLaunchMetadata::default()).unwrap();
        payload["schema_version"] = serde_json::json!(999);
        let payload = serde_json::to_string(&payload).unwrap();
        db.conn
            .execute(
                "UPDATE thread_runtime SET launch_metadata = ?2
                 WHERE thread_id = ?1",
                params!["t1", payload],
            )
            .unwrap();
        let runtime = db.get_runtime_info("t1").unwrap().unwrap();
        assert!(runtime.launch_metadata.is_none());
        assert_eq!(
            runtime.incompatible_launch_metadata,
            Some(IncompatibleLaunchMetadata {
                schema_version: 999,
                admitted_launch_capsule_schema: None,
            })
        );
    }

    #[test]
    fn launch_claim_first_caller_wins_second_blocked() {
        let (_tmp, db) = fresh_db();
        // Fresh thread: first owner wins.
        assert_eq!(
            db.claim_thread_launch("t1", "c1", "daemon-a").unwrap(),
            LaunchClaimOutcome::Claimed
        );
        // A second launcher cannot time-reclaim active daemon ownership.
        assert_eq!(
            db.claim_thread_launch("t1", "c2", "daemon-b").unwrap(),
            LaunchClaimOutcome::AlreadyClaimed
        );
        // The live claim still belongs to the first caller.
        let claim = db.get_launch_claim("t1").unwrap().expect("claim present");
        assert_eq!(claim.claim_id, "c1");
        assert_eq!(claim.owner.thread_id, "t1");
        assert_eq!(claim.owner.unpredictable_nonce, "c1");
        assert_eq!(claim.owner.daemon_generation_id, "daemon-a");
        assert_eq!(
            claim.claimed_by,
            lillux::canonical_json(&serde_json::to_value(&claim.owner).unwrap()).unwrap()
        );
    }

    #[test]
    fn launch_claim_does_not_expire_within_daemon_lifetime() {
        let (_tmp, db) = fresh_db();
        assert_eq!(
            db.claim_thread_launch("t1", "c1", "daemon-a").unwrap(),
            LaunchClaimOutcome::Claimed
        );
        assert_eq!(
            db.claim_thread_launch("t1", "c2", "daemon-b").unwrap(),
            LaunchClaimOutcome::AlreadyClaimed,
            "wall-clock time must never authorize a duplicate spawn"
        );
        let claim = db.get_launch_claim("t1").unwrap().expect("claim present");
        assert_eq!(claim.claim_id, "c1");
        assert_eq!(claim.owner.thread_id, "t1");
        assert_eq!(claim.owner.unpredictable_nonce, "c1");
        assert_eq!(claim.owner.daemon_generation_id, "daemon-a");
        assert_eq!(
            claim.claimed_by,
            lillux::canonical_json(&serde_json::to_value(&claim.owner).unwrap()).unwrap()
        );
        assert_eq!(claim.lease_expires_at_ms, i64::MAX);
    }

    #[test]
    fn startup_sweep_clears_only_dead_generation_claims() {
        let (_tmp, db) = fresh_db();
        assert_eq!(
            db.claim_thread_launch("t-dead", "c-dead", "daemon-old")
                .unwrap(),
            LaunchClaimOutcome::Claimed
        );
        assert_eq!(
            db.claim_thread_launch("t-live", "c-live", "daemon-current")
                .unwrap(),
            LaunchClaimOutcome::Claimed
        );

        let cleared = db.clear_stale_launch_claims("daemon-current").unwrap();
        assert_eq!(cleared.len(), 1);
        assert_eq!(cleared[0].thread_id, "t-dead");
        assert_eq!(cleared[0].dead_generation, "daemon-old");

        // The live claim survives; the dead thread is claimable again and its
        // epoch fencing still advances monotonically.
        assert!(db.get_launch_claim("t-live").unwrap().is_some());
        assert!(db.get_launch_claim("t-dead").unwrap().is_none());
        assert_eq!(
            db.claim_thread_launch("t-dead", "c-new", "daemon-current")
                .unwrap(),
            LaunchClaimOutcome::Claimed
        );
        let reclaimed = db.get_launch_claim("t-dead").unwrap().expect("reclaimed");
        assert_eq!(reclaimed.owner.monotonic_launch_epoch, 2);

        // Idempotent: a second sweep finds nothing.
        assert!(
            db.clear_stale_launch_claims("daemon-current")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn startup_sweep_clears_malformed_owner_rows_fail_closed() {
        let (_tmp, db) = fresh_db();
        db.conn
            .execute(
                "INSERT INTO thread_launch_claim
                     (thread_id, claim_id, claimed_at_ms, lease_expires_at_ms, claimed_by)
                 VALUES ('t-junk', 'c-junk', 0, 9223372036854775807, 'not json')",
                [],
            )
            .unwrap();
        let cleared = db.clear_stale_launch_claims("daemon-current").unwrap();
        assert_eq!(cleared.len(), 1);
        assert_eq!(cleared[0].dead_generation, "<malformed owner>");
        assert_eq!(
            db.claim_thread_launch("t-junk", "c-new", "daemon-current")
                .unwrap(),
            LaunchClaimOutcome::Claimed
        );
    }

    #[test]
    fn launch_claim_release_frees_for_reclaim() {
        let (_tmp, db) = fresh_db();
        assert_eq!(
            db.claim_thread_launch("t1", "c1", "daemon-a").unwrap(),
            LaunchClaimOutcome::Claimed
        );
        // A mismatched claim_id must not delete another owner's claim.
        assert!(!db.release_thread_launch_claim("t1", "other").unwrap());
        assert!(db.get_launch_claim("t1").unwrap().is_some());
        // The owner releases; the thread becomes immediately reclaimable.
        assert!(db.release_thread_launch_claim("t1", "c1").unwrap());
        assert!(db.get_launch_claim("t1").unwrap().is_none());
        assert_eq!(
            db.claim_thread_launch("t1", "c2", "daemon-b").unwrap(),
            LaunchClaimOutcome::Claimed
        );
    }

    fn seed_follow(key: &str) -> NewFollowWaiter {
        NewFollowWaiter {
            follow_key: key.to_string(),
            parent_thread_id: "parent-1".to_string(),
            parent_chain_root_id: "chain-parent".to_string(),
            follow_node: "n_follow".to_string(),
            graph_run_id: "gr-1".to_string(),
            step_count: 3,
            frontier_id: None,
            fanout: false,
            expected_children: 1,
            child_project_authority: None,
        }
    }

    fn set_single_follow_child(
        db: &RuntimeDb,
        follow_key: &str,
        child_thread_id: &str,
        child_chain_root_id: &str,
    ) -> Result<()> {
        let sealed = crate::thread_lifecycle::SealedRootExecutionRequest::storage_test_fixture();
        let item_ref = sealed.item_ref();
        let parameters = serde_json::json!({});
        db.set_follow_child(
            follow_key,
            0,
            item_ref,
            &follow_child_spec_hash(item_ref, &BTreeMap::new(), &parameters, None).unwrap(),
            child_thread_id,
            child_chain_root_id,
            &sealed,
        )
    }

    #[test]
    fn reserve_follow_is_idempotent() {
        let (_tmp, db) = fresh_db();
        let a = db.reserve_follow(&seed_follow("fk1")).unwrap();
        assert_eq!(a.phase, follow_phase::RESERVED);
        let b = db.reserve_follow(&seed_follow("fk1")).unwrap();
        // ON CONFLICT DO NOTHING ⇒ same row, not a second insert.
        assert_eq!(b.created_at_ms, a.created_at_ms);
        assert_eq!(db.list_follow_waiters().unwrap().len(), 1);
    }

    #[test]
    fn follow_waiter_full_lifecycle() {
        let (_tmp, db) = fresh_db();
        db.reserve_follow(&seed_follow("fk1")).unwrap();
        set_single_follow_child(&db, "fk1", "child-1", "chain-child").unwrap();
        db.set_follow_parent_successor("fk1", "succ-1").unwrap();
        db.mark_follow_waiting("fk1").unwrap();

        let w = db.get_follow_waiter_by_key("fk1").unwrap().unwrap();
        assert_eq!(w.phase, follow_phase::WAITING);
        assert_eq!(w.children[0].child_chain_root_id, "chain-child");
        assert_eq!(w.parent_successor_thread_id.as_deref(), Some("succ-1"));

        // Lookup by child chain (the terminal-hook path).
        let by_child = db
            .get_follow_waiter_by_child_chain("chain-child")
            .unwrap()
            .unwrap();
        assert_eq!(by_child.follow_key, "fk1");

        // Mark terminal by child chain stores the envelope and flips to ready.
        let envelope =
            serde_json::json!({"success": true, "status": "completed", "result": {"x": 1}});
        let matched = db
            .mark_follow_child_terminal("chain-child", "child-tail", "completed", &envelope)
            .unwrap();
        assert!(matched);
        let ready = db.get_follow_waiter_by_key("fk1").unwrap().unwrap();
        assert_eq!(ready.phase, follow_phase::READY);
        assert_eq!(
            ready.children[0].terminal_status.as_deref(),
            Some("completed")
        );
        assert_eq!(ready.children[0].terminal_envelope, Some(envelope));

        db.clear_follow_waiter("fk1").unwrap();
        assert!(db.get_follow_waiter_by_key("fk1").unwrap().is_none());
        assert!(db.list_follow_waiters().unwrap().is_empty());
        assert!(db.get_follow_child("fk1", 0).unwrap().is_none());
    }

    #[test]
    fn follow_cohort_flips_ready_only_after_last_ordered_child() {
        let (_tmp, db) = fresh_db();
        let mut seed = seed_follow("fk-cohort");
        seed.fanout = true;
        seed.expected_children = 2;
        db.reserve_follow(&seed).unwrap();

        let params_0 = serde_json::json!({"episode": 0});
        let params_1 = serde_json::json!({"episode": 1});
        let sealed = crate::thread_lifecycle::SealedRootExecutionRequest::storage_test_fixture();
        let item_ref = sealed.item_ref();
        db.set_follow_child(
            "fk-cohort",
            0,
            item_ref,
            &follow_child_spec_hash(item_ref, &BTreeMap::new(), &params_0, None).unwrap(),
            "child-0",
            "chain-0",
            &sealed,
        )
        .unwrap();
        db.set_follow_parent_successor("fk-cohort", "succ-1")
            .unwrap();
        assert!(db.mark_follow_waiting("fk-cohort").is_err());

        db.set_follow_child(
            "fk-cohort",
            1,
            item_ref,
            &follow_child_spec_hash(item_ref, &BTreeMap::new(), &params_1, None).unwrap(),
            "child-1",
            "chain-1",
            &sealed,
        )
        .unwrap();
        db.mark_follow_waiting("fk-cohort").unwrap();

        let envelope_1 = serde_json::json!({"success": true, "result": 1});
        assert!(
            !db.mark_follow_child_terminal("chain-1", "tail-1", "completed", &envelope_1)
                .unwrap()
        );
        assert_eq!(
            db.get_follow_waiter_by_key("fk-cohort")
                .unwrap()
                .unwrap()
                .phase,
            follow_phase::WAITING
        );

        let envelope_0 = serde_json::json!({"success": true, "result": 0});
        assert!(
            db.mark_follow_child_terminal("chain-0", "tail-0", "completed", &envelope_0)
                .unwrap()
        );
        let ready = db.get_follow_waiter_by_key("fk-cohort").unwrap().unwrap();
        assert_eq!(ready.phase, follow_phase::READY);
        assert_eq!(ready.children[0].item_index, 0);
        assert_eq!(ready.children[1].item_index, 1);
        assert_eq!(ready.children[0].terminal_envelope, Some(envelope_0));
        assert_eq!(ready.children[1].terminal_envelope, Some(envelope_1));
    }

    #[test]
    fn follow_child_spec_is_immutable_per_index() {
        let (_tmp, db) = fresh_db();
        db.reserve_follow(&seed_follow("fk1")).unwrap();
        let first = serde_json::json!({"episode": 1});
        let changed = serde_json::json!({"episode": 2});
        let sealed = crate::thread_lifecycle::SealedRootExecutionRequest::storage_test_fixture();
        let item_ref = sealed.item_ref();
        db.set_follow_child(
            "fk1",
            0,
            item_ref,
            &follow_child_spec_hash(item_ref, &BTreeMap::new(), &first, None).unwrap(),
            "child-1",
            "chain-1",
            &sealed,
        )
        .unwrap();
        assert!(
            db.set_follow_child(
                "fk1",
                0,
                item_ref,
                &follow_child_spec_hash(item_ref, &BTreeMap::new(), &changed, None,).unwrap(),
                "child-1",
                "chain-1",
                &sealed,
            )
            .is_err()
        );
    }

    #[test]
    fn lookup_by_parent_and_successor_thread() {
        let (_tmp, db) = fresh_db();
        db.reserve_follow(&seed_follow("fk1")).unwrap();
        set_single_follow_child(&db, "fk1", "child-1", "chain-child").unwrap();
        db.set_follow_parent_successor("fk1", "succ-1").unwrap();
        db.mark_follow_waiting("fk1").unwrap();

        // Suspended-parent decoration: found by the issuing parent thread.
        let by_parent = db
            .get_follow_waiter_by_parent_thread("parent-1")
            .unwrap()
            .unwrap();
        assert_eq!(by_parent.follow_key, "fk1");
        assert_eq!(by_parent.phase, follow_phase::WAITING);

        // Resume-successor decoration: found by the recorded successor thread.
        let by_succ = db
            .get_follow_waiter_by_successor("succ-1")
            .unwrap()
            .unwrap();
        assert_eq!(by_succ.follow_key, "fk1");

        // Unrelated ids miss.
        assert!(
            db.get_follow_waiter_by_parent_thread("nope")
                .unwrap()
                .is_none()
        );
        assert!(db.get_follow_waiter_by_successor("nope").unwrap().is_none());

        // Cleared waiter is invisible to both accessors (terminal history moves
        // to the projection's continuation edge).
        db.clear_follow_waiter("fk1").unwrap();
        assert!(
            db.get_follow_waiter_by_parent_thread("parent-1")
                .unwrap()
                .is_none()
        );
        assert!(
            db.get_follow_waiter_by_successor("succ-1")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn list_waiter_summary_is_scoped_bounded_and_ignores_terminal_envelope() {
        let (_tmp, db) = fresh_db();
        db.reserve_follow(&seed_follow("fk1")).unwrap();
        set_single_follow_child(&db, "fk1", "child-1", "chain-child").unwrap();
        db.set_follow_parent_successor("fk1", "succ-1").unwrap();
        db.mark_follow_waiting("fk1").unwrap();
        // A corrupt or oversized terminal envelope is reconciliation data. The
        // list projection must not fetch or decode it.
        db.conn
            .execute(
                "UPDATE follow_waiter_child \
                 SET terminal_status = 'completed', terminal_envelope = '{not-json' \
                 WHERE follow_key = 'fk1'",
                [],
            )
            .unwrap();

        let requested = vec!["unrelated".to_string(), "succ-1".to_string()];
        let summaries = db
            .follow_waiter_summaries_for_threads(&requested, 2)
            .unwrap();
        assert_eq!(summaries.len(), 1);
        let summary = &summaries[0];
        assert_eq!(summary.parent_thread_id, "parent-1");
        assert_eq!(summary.first_child_thread_id.as_deref(), Some("child-1"));
        assert_eq!(
            summary.first_child_terminal_status.as_deref(),
            Some("completed")
        );
        assert!(summary.all_children_terminal());

        db.reserve_follow(&seed_follow("fk2")).unwrap();
        assert!(db.follow_waiter_summaries_bounded(1).is_err());
    }

    #[test]
    fn mark_terminal_unknown_chain_is_no_match() {
        let (_tmp, db) = fresh_db();
        let matched = db
            .mark_follow_child_terminal("nope", "t", "completed", &serde_json::json!({}))
            .unwrap();
        assert!(!matched);
    }

    #[test]
    fn child_chain_root_is_unique_across_follows() {
        let (_tmp, db) = fresh_db();
        db.reserve_follow(&seed_follow("fk1")).unwrap();
        db.reserve_follow(&seed_follow("fk2")).unwrap();
        set_single_follow_child(&db, "fk1", "child-1", "shared-chain").unwrap();
        // A second follow cannot claim the same child chain root (UNIQUE).
        assert!(
            set_single_follow_child(&db, "fk2", "child-2", "shared-chain").is_err(),
            "duplicate child_chain_root_id must violate UNIQUE"
        );
    }

    #[test]
    fn invalid_phase_rejected_by_check() {
        let (_tmp, db) = fresh_db();
        db.reserve_follow(&seed_follow("fk1")).unwrap();
        // Exercise the CHECK constraint via a raw update.
        assert!(
            db.conn
                .execute(
                    "UPDATE follow_waiter SET phase = 'bogus' WHERE follow_key = 'fk1'",
                    [],
                )
                .is_err(),
            "CHECK must reject an unknown phase"
        );
    }

    #[test]
    fn reserve_follow_rejects_conflicting_seed() {
        let (_tmp, db) = fresh_db();
        db.reserve_follow(&seed_follow("fk1")).unwrap();
        let mut conflicting = seed_follow("fk1");
        conflicting.step_count = 99; // same key, different follow point
        assert!(db.reserve_follow(&conflicting).is_err());
    }

    #[test]
    fn set_follow_child_refuses_conflicting_overwrite() {
        let (_tmp, db) = fresh_db();
        db.reserve_follow(&seed_follow("fk1")).unwrap();
        set_single_follow_child(&db, "fk1", "child-1", "chain-1").unwrap();
        set_single_follow_child(&db, "fk1", "child-1", "chain-1").unwrap();
        assert!(set_single_follow_child(&db, "fk1", "child-2", "chain-2").is_err());
    }

    #[test]
    fn set_follow_parent_successor_refuses_conflicting_overwrite() {
        let (_tmp, db) = fresh_db();
        db.reserve_follow(&seed_follow("fk1")).unwrap();
        db.set_follow_parent_successor("fk1", "succ-1").unwrap();
        db.set_follow_parent_successor("fk1", "succ-1").unwrap(); // idempotent
        assert!(db.set_follow_parent_successor("fk1", "succ-2").is_err());
    }

    #[test]
    fn phase_transitions_are_constrained() {
        let (_tmp, db) = fresh_db();
        db.reserve_follow(&seed_follow("fk1")).unwrap();
        // Cannot mark waiting before child + successor recorded.
        assert!(db.mark_follow_waiting("fk1").is_err());
        set_single_follow_child(&db, "fk1", "c", "chain-1").unwrap();
        db.set_follow_parent_successor("fk1", "succ-1").unwrap();
        db.mark_follow_waiting("fk1").unwrap();
        // Cannot resume from waiting (must be ready first).
        assert!(db.mark_follow_resuming("fk1").is_err());
        db.mark_follow_child_terminal(
            "chain-1",
            "c-tail",
            "completed",
            &serde_json::json!({"ok": true}),
        )
        .unwrap();
        db.mark_follow_resuming("fk1").unwrap();
        // A late/duplicate terminal hook must NOT downgrade resuming → ready.
        let matched = db
            .mark_follow_child_terminal(
                "chain-1",
                "c-tail",
                "completed",
                &serde_json::json!({"ok": true}),
            )
            .unwrap();
        assert!(
            !matched,
            "resuming row must not be downgraded by a late terminal"
        );
        assert_eq!(
            db.get_follow_waiter_by_key("fk1").unwrap().unwrap().phase,
            follow_phase::RESUMING
        );
    }

    #[test]
    fn corrupt_terminal_envelope_json_fails_read() {
        let (_tmp, db) = fresh_db();
        db.reserve_follow(&seed_follow("fk1")).unwrap();
        set_single_follow_child(&db, "fk1", "c", "chain-1").unwrap();
        db.conn
            .execute(
                "UPDATE follow_waiter_child
                    SET terminal_envelope = '{not json'
                  WHERE follow_key = 'fk1' AND item_index = 0",
                [],
            )
            .unwrap();
        assert!(db.get_follow_waiter_by_key("fk1").is_err());
    }

    #[test]
    fn ready_terminal_result_is_immutable() {
        let (_tmp, db) = fresh_db();
        db.reserve_follow(&seed_follow("fk1")).unwrap();
        set_single_follow_child(&db, "fk1", "c", "chain-1").unwrap();
        db.set_follow_parent_successor("fk1", "succ-1").unwrap();
        db.mark_follow_waiting("fk1").unwrap();

        let env_a = serde_json::json!({"success": true, "result": "A"});
        assert!(
            db.mark_follow_child_terminal("chain-1", "c-tail", "completed", &env_a)
                .unwrap()
        );
        // Same data again: idempotent no-op (no error, no rewrite).
        assert!(
            !db.mark_follow_child_terminal("chain-1", "c-tail", "completed", &env_a)
                .unwrap()
        );
        // Conflicting terminal data is refused; the row keeps the first result.
        let env_b = serde_json::json!({"success": false, "result": "B"});
        assert!(
            db.mark_follow_child_terminal("chain-1", "c-other", "failed", &env_b)
                .is_err()
        );
        let w = db.get_follow_waiter_by_key("fk1").unwrap().unwrap();
        assert_eq!(w.children[0].terminal_envelope, Some(env_a));
        assert_eq!(w.children[0].terminal_thread_id.as_deref(), Some("c-tail"));
        assert_eq!(w.children[0].terminal_status.as_deref(), Some("completed"));
    }

    #[test]
    fn launch_planning_cancel_wins_only_while_unbound() {
        let (_tmp, db) = fresh_db();
        db.reserve_launch_planning("L-one", "T-one", "fp:owner")
            .unwrap();
        assert!(db.cancel_unbound_launch_planning("L-one").unwrap());
        assert!(!db.bind_launch_planning("T-one").unwrap());
        let record = db.launch_planning_by_id("L-one").unwrap().unwrap();
        assert_eq!(record.state, "cancelled");
        assert_eq!(record.requested_by, "fp:owner");
        assert!(record.bound_thread_id.is_none());
    }

    #[test]
    fn launch_planning_bind_fences_late_cancel() {
        let (_tmp, db) = fresh_db();
        db.reserve_launch_planning("L-two", "T-two", "fp:owner")
            .unwrap();
        assert!(db.bind_launch_planning("T-two").unwrap());
        assert!(!db.cancel_unbound_launch_planning("L-two").unwrap());
        let record = db.launch_planning_by_id("L-two").unwrap().unwrap();
        assert_eq!(record.state, "bound");
        assert_eq!(record.bound_thread_id.as_deref(), Some("T-two"));
    }

    #[test]
    fn bound_launch_planning_lives_with_authoritative_chain_history() {
        let (_tmp, db) = fresh_db();
        db.reserve_launch_planning("L-bound", "T-bound", "fp:owner")
            .unwrap();
        assert!(db.bind_launch_planning("T-bound").unwrap());
        db.conn
            .execute(
                "UPDATE launch_planning SET finished_at_ms = 1 WHERE launch_id = 'L-bound'",
                [],
            )
            .unwrap();
        // More than the terminal-row cap cannot evict a bound coordinate.
        for index in 0..4_100 {
            db.conn
                .execute(
                    "INSERT INTO launch_planning (
                        launch_id, reserved_thread_id, requested_by,
                        daemon_generation_id, state, created_at_ms,
                        updated_at_ms, finished_at_ms, outcome_code
                     ) VALUES (?1, ?2, 'fp:owner', 'generation', 'failed', 1, 1, 1, 'failed')",
                    params![format!("L-failed-{index}"), format!("T-failed-{index}")],
                )
                .unwrap();
        }
        prune_launch_planning(&db.conn, 24 * 60 * 60 * 1_000 + 2).unwrap();
        assert!(db.launch_planning_by_id("L-bound").unwrap().is_some());

        db.delete_chain_runtime("T-bound", &["T-bound".to_string()])
            .unwrap();
        assert!(db.launch_planning_by_id("L-bound").unwrap().is_none());
    }

    #[test]
    fn restart_generation_expires_only_unbound_predecessor_planning() {
        let (_tmp, db) = fresh_db();
        db.reserve_launch_planning("L-stale", "T-stale", "fp:owner")
            .unwrap();
        db.reserve_launch_planning("L-current", "T-current", "fp:owner")
            .unwrap();
        db.conn
            .execute(
                "UPDATE launch_planning
                    SET daemon_generation_id = 'previous-daemon-generation'
                  WHERE launch_id = 'L-stale'",
                [],
            )
            .unwrap();

        assert_eq!(db.expire_stale_launch_planning().unwrap(), 1);
        let stale = db.launch_planning_by_id("L-stale").unwrap().unwrap();
        assert_eq!(stale.state, "expired");
        assert_eq!(
            stale.outcome_code.as_deref(),
            Some("daemon_restarted_before_thread_bind")
        );
        let current = db.launch_planning_by_id("L-current").unwrap().unwrap();
        assert_eq!(current.state, "planning");
        assert!(current.outcome_code.is_none());
    }

    #[test]
    fn launch_planning_pending_admission_is_bounded_and_terminal_rows_release_capacity() {
        let (_tmp, db) = fresh_db();
        db.reserve_launch_planning_bounded("L-one", "T-one", "fp:owner", 2)
            .unwrap();
        db.reserve_launch_planning_bounded("L-two", "T-two", "fp:owner", 2)
            .unwrap();
        let capacity = db
            .reserve_launch_planning_bounded("L-three", "T-three", "fp:owner", 2)
            .expect_err("third pending launch must reach bounded capacity");
        assert!(capacity.is::<LaunchPlanningCapacityExceeded>());

        assert!(db.cancel_unbound_launch_planning("L-one").unwrap());
        db.reserve_launch_planning_bounded("L-three", "T-three", "fp:owner", 2)
            .unwrap();
        assert_eq!(db.pending_launch_planning().unwrap().len(), 2);
    }

    #[test]
    fn launch_planning_coordinate_cannot_be_reused() {
        let (_tmp, db) = fresh_db();
        db.reserve_launch_planning("L-one", "T-one", "fp:owner")
            .unwrap();
        let error = db
            .reserve_launch_planning("L-one", "T-two", "fp:owner")
            .expect_err("one coordinate cannot name two launch attempts");
        assert!(error.is::<LaunchPlanningAlreadyReserved>());
        let retained = db.launch_planning_by_id("L-one").unwrap().unwrap();
        assert_eq!(retained.reserved_thread_id, "T-one");
    }
}
