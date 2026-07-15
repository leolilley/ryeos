use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::path::Path;

use anyhow::{bail, Context, Result};
use rusqlite::{
    params, Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
};
use serde::Serialize;
use serde_json::Value;

use crate::launch_metadata::{RuntimeLaunchMetadata, LAUNCH_METADATA_SCHEMA_VERSION};
use crate::process::{
    validate_execution_process_identity_shape, ExecutionProcessIdentity,
    PROCESS_IDENTITY_SCHEMA_VERSION,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopIntent {
    Cancel,
    Kill,
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
    pub stop_requested_at_ms: Option<i64>,
    #[serde(skip_serializing)]
    pub stop_intent: Option<StopIntent>,
    /// Internal recovery/resume authority. It can retain the original free-form
    /// execution parameters, so it must never be echoed through ThreadDetail or
    /// another service response. Internal owners use `get_launch_metadata`.
    #[serde(skip_serializing)]
    pub launch_metadata: Option<RuntimeLaunchMetadata>,
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
const CONTINUATION_SEED_MARKER: &[u8] = b"continuation_seed_v1";
pub(crate) const CONTINUATION_SEED_RECONCILE_PAGE_SIZE: usize = 512;

/// A live thread cannot accumulate unbounded terminalization work.
pub const MAX_OPEN_COMMANDS_PER_THREAD: usize = 128;
/// Aggregate variable content retained by a thread's open commands.
pub const MAX_OPEN_COMMAND_CONTENT_BYTES: usize = 4 * 1024 * 1024;

/// Maximum exact callback response retained for a completed hook dispatch.
/// This remains below the callback UDS frame budget and prevents the ledger
/// from becoming an unbounded response store.
pub const MAX_HOOK_DISPATCH_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct NewHookDispatch {
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
    Replay(Value),
    PendingUnknown,
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

/// A live launch claim, as read back for reconcile/inspection.
#[derive(Debug, Clone)]
pub struct LaunchClaim {
    pub thread_id: String,
    pub claim_id: String,
    pub claimed_at_ms: i64,
    pub lease_expires_at_ms: i64,
    pub claimed_by: String,
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
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;

CREATE TABLE IF NOT EXISTS thread_runtime (
    thread_id TEXT PRIMARY KEY,
    chain_root_id TEXT NOT NULL,
    pid INTEGER,
    pgid INTEGER,
    metadata BLOB,
    launch_metadata TEXT,
    resume_attempts INTEGER NOT NULL DEFAULT 0,
    process_identity TEXT,
    stop_requested_at_ms INTEGER,
    stop_intent TEXT
);

CREATE INDEX IF NOT EXISTS idx_thread_runtime_chain_root
    ON thread_runtime(chain_root_id);

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

CREATE TABLE IF NOT EXISTS thread_launch_claim (
    thread_id TEXT PRIMARY KEY,
    claim_id TEXT NOT NULL,
    claimed_at_ms INTEGER NOT NULL,
    lease_expires_at_ms INTEGER NOT NULL,
    claimed_by TEXT NOT NULL
);

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
    expected_children INTEGER NOT NULL DEFAULT 1
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
"#;

use ryeos_state::sqlite_schema;

/// Application ID stamp for `runtime.sqlite3`.
/// RYEA = 0x5259_4541 ("RY" + "EA" for "runtime").
const RUNTIME_APP_ID: i32 = 0x5259_4541;

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
        ],
        indexes: &[
            sqlite_schema::IndexSpec {
                name: "idx_thread_runtime_chain_root",
                table: "thread_runtime",
                columns: &["chain_root_id"],
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
        ],
    }
}

pub struct RuntimeDb {
    conn: Connection,
    _directory: Option<lillux::PinnedDirectory>,
    _directory_lock: Option<lillux::secure_fs::PinnedDirectoryLock>,
    _database_file: Option<File>,
    _wal_file: Option<File>,
    _shm_file: Option<File>,
}

fn assert_current_runtime_schema(conn: &Connection, path: &Path) -> Result<()> {
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

impl RuntimeDb {
    pub fn new_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("failed to open in-memory runtime db")?;
        let spec = runtime_schema_spec();
        sqlite_schema::init_owned(&conn, &spec, SCHEMA_SQL, Path::new(":memory:"))?;
        assert_current_runtime_schema(&conn, Path::new(":memory:"))?;
        Ok(Self {
            conn,
            _directory: None,
            _directory_lock: None,
            _database_file: None,
            _wal_file: None,
            _shm_file: None,
        })
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
                dispatch_key, chain_root_id, caller_thread_id, event, hook_id,
                request_hash, status, created_at_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                seed.dispatch_key,
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

        let (request_hash, status, response_json, response_hash): (
            String,
            String,
            Option<Vec<u8>>,
            Option<String>,
        ) = tx.query_row(
            "SELECT request_hash, status, response_json, response_hash
               FROM hook_dispatch_ledger WHERE dispatch_key=?1",
            params![seed.dispatch_key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        if request_hash != seed.request_hash {
            bail!(
                "hook dispatch key `{}` was reused with a different request hash",
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
                HookDispatchReservation::Replay(decode_completed_hook_response(
                    &seed.dispatch_key,
                    response_json.as_deref(),
                    response_hash.as_deref(),
                )?)
            }
        };
        tx.commit()?;
        Ok(reservation)
    }

    /// Seal the exact callback response for a previously reserved dispatch.
    /// Repeating the same completion is harmless; any divergent completion is
    /// an integrity failure.
    pub fn complete_hook_dispatch(
        &self,
        dispatch_key: &str,
        request_hash: &str,
        response: &Value,
    ) -> Result<()> {
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
        let existing: Option<(String, String, Option<Vec<u8>>, Option<String>)> = tx
            .query_row(
                "SELECT request_hash, status, response_json, response_hash
                   FROM hook_dispatch_ledger WHERE dispatch_key=?1",
                params![dispatch_key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((stored_request_hash, status, stored_json, stored_hash)) = existing else {
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
        Ok(())
    }

    pub fn open(path: &Path) -> Result<Self> {
        Self::open_bound(path, true)
    }

    /// Open the persisted runtime database for offline projection recovery
    /// without creating or migrating anything. Pending head transitions use
    /// this as fail-closed liveness authority, so an absent or stale database
    /// must never be replaced by a fresh empty one.
    pub fn open_existing_current(path: &Path) -> Result<Self> {
        Self::open_bound(path, false)
    }

    fn open_bound(path: &Path, allow_create: bool) -> Result<Self> {
        let name = path
            .file_name()
            .ok_or_else(|| {
                anyhow::anyhow!("runtime database path has no filename: {}", path.display())
            })?
            .to_os_string();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let directory = if allow_create {
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
        inspect_runtime_sidecars(&directory, &name)?;

        let existing = directory.open_regular(&name, true).with_context(|| {
            format!(
                "runtime database must be a regular non-symlink file: {}",
                path.display()
            )
        })?;
        let (database_file, created) = match existing {
            Some(file) => (file, false),
            None if allow_create => {
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
            sqlite_schema::init_owned(&conn, &runtime_schema_spec(), SCHEMA_SQL, path)?;
        }
        assert_current_runtime_schema(&conn, path)?;
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .context("verify runtime database integrity")?;
        if integrity != "ok" {
            bail!(
                "runtime database integrity check failed for {}: {integrity}",
                path.display()
            );
        }
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
            _directory: Some(directory),
            _directory_lock: Some(directory_lock),
            _database_file: Some(database_file),
            _wal_file: Some(wal_file),
            _shm_file: Some(shm_file),
        })
    }

    pub fn insert_thread_runtime(&self, thread_id: &str, chain_root_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO thread_runtime (thread_id, chain_root_id, pid, pgid, metadata, launch_metadata)
             VALUES (?1, ?2, NULL, NULL, NULL, NULL)",
            params![thread_id, chain_root_id],
        )?;
        Ok(())
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
        let launch_metadata_json = serde_json::to_string(launch_metadata)
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
        let launch_metadata_json = serde_json::to_string(launch_metadata)
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
            "DELETE FROM thread_runtime WHERE thread_id = ?1",
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

    pub fn delete_chain_runtime(
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
                deleted += self.conn.execute(
                    "DELETE FROM thread_commands WHERE thread_id=?1",
                    params![&thread_id],
                )?;
                deleted += self.conn.execute(
                    "DELETE FROM thread_launch_claim WHERE thread_id=?1",
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
                "SELECT pid, pgid, process_identity, stop_requested_at_ms
                   FROM thread_runtime WHERE thread_id = ?1",
                params![thread_id],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| {
                anyhow::anyhow!("thread_runtime row missing for thread_id: {thread_id}")
            })?;
        let (existing_pid, existing_pgid, existing_identity, stop_requested_at_ms) = existing;
        if let Some(existing_identity) = existing_identity {
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
            if stop_requested_at_ms.is_none() && !launch_metadata.is_empty() {
                let lm_json = serde_json::to_string(launch_metadata)
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
        if launch_metadata.is_empty() {
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
        }
        let lm_json =
            serde_json::to_string(launch_metadata).context("failed to encode launch_metadata")?;
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
                SET pid = NULL, pgid = NULL, process_identity = NULL
              WHERE thread_id = ?1 AND process_identity = ?2",
            params![thread_id, identity_json],
        )? > 0)
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
        let lm_json =
            serde_json::to_string(launch_metadata).context("failed to encode launch_metadata")?;
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
                        stop_requested_at_ms, stop_intent
                   FROM thread_runtime WHERE thread_id = ?1",
                params![thread_id],
                |row| {
                    let pid: Option<i64> = row.get(0)?;
                    let pgid: Option<i64> = row.get(1)?;
                    let lm_text: Option<String> = row.get(2)?;
                    let identity_text: Option<String> = row.get(3)?;
                    let stop_requested_at_ms: Option<i64> = row.get(4)?;
                    let stop_intent: Option<String> = row.get(5)?;
                    Ok((
                        pid,
                        pgid,
                        lm_text,
                        identity_text,
                        stop_requested_at_ms,
                        stop_intent,
                    ))
                },
            )
            .optional()?;
        let Some((pid, pgid, lm_text, identity_text, stop_requested_at_ms, stop_intent)) = raw
        else {
            return Ok(None);
        };
        let launch_metadata = match lm_text.as_deref() {
            None => None,
            Some(s) => match serde_json::from_str::<RuntimeLaunchMetadata>(s) {
                Ok(m) => {
                    if m.schema_version != LAUNCH_METADATA_SCHEMA_VERSION {
                        bail!(
                            "launch_metadata schema_version mismatch for thread {thread_id}: \
                             persisted={}; expected={}; payload_len={}. \
                             Refusing to operate on stale schema. \
                             Recovery: mv <db_file> <db_file>.foreign.$(date +%s); \
                             then restart the daemon (auto-init will recreate missing state).",
                            m.schema_version,
                            LAUNCH_METADATA_SCHEMA_VERSION,
                            s.len(),
                        );
                    } else {
                        Some(m)
                    }
                }
                Err(err) => {
                    // Do NOT log raw payload — ResumeContext.parameters
                    // can contain user/tool params that may include secrets.
                    bail!(
                        "failed to decode launch_metadata for thread {thread_id}: {err:#} \
                         (payload_len={}). Corrupt or foreign row. \
                         Recovery: mv <db_file> <db_file>.foreign.$(date +%s); \
                         then restart the daemon (auto-init will recreate missing state).",
                        s.len(),
                    );
                }
            },
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
        let stop_intent = stop_intent.as_deref().map(StopIntent::parse).transpose()?;
        if stop_requested_at_ms.is_some() != stop_intent.is_some() {
            bail!(
                "incomplete durable stop tombstone for thread {thread_id}: timestamp and intent must be present together"
            );
        }
        Ok(Some(RuntimeInfo {
            pid,
            pgid,
            process_identity,
            stop_requested_at_ms,
            stop_intent,
            launch_metadata,
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
        claimed_by: &str,
    ) -> Result<LaunchClaimOutcome> {
        let now_ms = lillux::time::timestamp_millis();
        // Keep the existing column at an explicit non-expiring sentinel so
        // diagnostics and pin readers retain one current-format shape.
        let changed = self.conn.execute(
            "INSERT INTO thread_launch_claim
                 (thread_id, claim_id, claimed_at_ms, lease_expires_at_ms, claimed_by)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(thread_id) DO NOTHING",
            params![thread_id, claim_id, now_ms, i64::MAX, claimed_by],
        )?;
        Ok(if changed == 1 {
            LaunchClaimOutcome::Claimed
        } else {
            LaunchClaimOutcome::AlreadyClaimed
        })
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

    /// Delete ALL launch claims. Called once at daemon startup (before reconcile
    /// dispatches): a restart proves every prior in-process launcher is gone, so
    /// every surviving claim is stale. Returns the count removed.
    pub fn clear_all_launch_claims(&self) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM thread_launch_claim", [])?)
    }

    /// Read the current launch claim for a thread, if any. The reconciler uses
    /// this to tell an unlaunched successor from one owned by a launch task.
    pub fn get_launch_claim(&self, thread_id: &str) -> Result<Option<LaunchClaim>> {
        self.conn
            .query_row(
                "SELECT thread_id, claim_id, claimed_at_ms, lease_expires_at_ms, claimed_by
                   FROM thread_launch_claim WHERE thread_id = ?1",
                params![thread_id],
                |row| {
                    Ok(LaunchClaim {
                        thread_id: row.get(0)?,
                        claim_id: row.get(1)?,
                        claimed_at_ms: row.get(2)?,
                        lease_expires_at_ms: row.get(3)?,
                        claimed_by: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
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
    /// [`Self::load_command`] this is not an error on absence — `commands.get`
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

    /// Record that `parent_thread_id` spawned `child_thread_id`. Idempotent on
    /// the child (a re-driven launch does not error or duplicate the link).
    ///
    /// `relation` is a descriptive tag only — the cascade walks every descendant
    /// regardless. The sole production caller records `"dispatch"` for both
    /// inline and follow children (they share one launch path); the value is
    /// reserved for a finer distinction if a consumer ever needs one.
    pub fn record_child_link(
        &self,
        parent_thread_id: &str,
        child_thread_id: &str,
        relation: &str,
    ) -> Result<()> {
        self.conn.execute(
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
        Ok(())
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
                 fanout, expected_children, phase, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'reserved', ?10, ?10)
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
        {
            bail!(
                "follow reservation conflict for follow_key {}: seed does not match the persisted row",
                seed.follow_key
            );
        }
        Ok(existing)
    }

    /// Record the spawned child's identities. Allowed only when unset (first
    /// write) or already equal (idempotent retry); never overwrites a different
    /// child, which would strand the original.
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
            bail!("follow waiter {follow_key} child index {item_index} conflicts with persisted child/spec");
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
                bail!("follow child chain {child_chain_root_id} has a partial persisted terminal tuple");
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
            let requested_rows = std::iter::repeat("(?)")
                .take(batch.len())
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

    fn launch_window_live_total(&self) -> Result<u32> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM launch_window WHERE launched_at_ms IS NOT NULL",
            [],
            |r| r.get(0),
        )?)
    }

    /// Admit queued members of one window, oldest first, up to the window
    /// width and the optional daemon-global live ceiling. Marks admitted
    /// rows launched and returns their chain roots — the caller owns
    /// actually launching them.
    pub fn launch_window_admit(
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
            if let Some(cap) = global_live_limit {
                if self.launch_window_live_total()? >= cap {
                    break;
                }
            }
            self.conn.execute(
                "UPDATE launch_window SET launched_at_ms = ?2 WHERE child_chain_root_id = ?1",
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
        let key: Option<String> = self
            .conn
            .query_row(
                "SELECT window_key FROM launch_window WHERE child_chain_root_id = ?1",
                params![child_chain_root_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(key) = key else {
            return Ok(Vec::new());
        };
        self.conn.execute(
            "DELETE FROM launch_window WHERE child_chain_root_id = ?1",
            params![child_chain_root_id],
        )?;
        self.launch_window_admit(&key, global_live_limit, now_ms)
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
     fanout, expected_children, phase, created_at_ms, updated_at_ms";

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
        children: Vec::new(),
        phase: row.get(10)?,
        created_at_ms: row.get(11)?,
        updated_at_ms: row.get(12)?,
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

    fn hook_seed() -> NewHookDispatch {
        NewHookDispatch {
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
        assert_eq!(replayed, response);
        db.complete_hook_dispatch(&seed.dispatch_key, &seed.request_hash, &response)
            .unwrap();
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
        assert_eq!(replayed, response);
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
        assert_eq!(replayed, response);
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
        assert!(db
            .complete_hook_dispatch(&seed.dispatch_key, &seed.request_hash, &divergent)
            .is_err());
    }

    #[test]
    fn hook_dispatch_rejects_invalid_oversize_and_corrupt_responses() {
        let (_tmp, db) = fresh_db();
        let seed = hook_seed();
        db.reserve_hook_dispatch(&seed).unwrap();
        assert!(db
            .complete_hook_dispatch(
                &seed.dispatch_key,
                &seed.request_hash,
                &serde_json::json!({"result": {}}),
            )
            .is_err());
        let oversize = serde_json::json!({
            "thread": {},
            "result": {"body": "x".repeat(MAX_HOOK_DISPATCH_RESPONSE_BYTES)}
        });
        assert!(db
            .complete_hook_dispatch(&seed.dispatch_key, &seed.request_hash, &oversize)
            .is_err());

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
        db.record_child_link("parent", "child", "inline").unwrap();
        // A re-driven launch of the same child must not error or duplicate.
        db.record_child_link("parent", "child", "inline").unwrap();
        assert_eq!(db.descendant_thread_ids("parent").unwrap(), vec!["child"]);
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
        assert!(settled
            .iter()
            .all(|r| r.completed_at.is_some() && r.result.is_some()));
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
        assert!(db
            .settle_open_commands("t1", "cancelled")
            .unwrap()
            .is_empty());
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
        assert!(db
            .complete_command(command.command_id, "completed", Some(&oversized))
            .is_err());
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
            assert!(db
                .submit_command(&NewCommandRecord {
                    thread_id: "invalid-command".to_string(),
                    command_type: command_type.to_string(),
                    requested_by: None,
                    params: None,
                })
                .is_err());
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
        assert!(db
            .settle_open_commands("settlement-bounds", &oversized_terminal_status)
            .is_err());
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
        assert!(db
            .submit_command(&NewCommandRecord {
                thread_id: "bounded-thread".to_string(),
                command_type: "cancel".to_string(),
                requested_by: None,
                params: None,
            })
            .is_err());
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
        let (tmp, _db) = fresh_db();
        let path = tmp.path().join("runtime.db");
        let _ = RuntimeDb::open(&path).unwrap();
        let _ = RuntimeDb::open(&path).unwrap();
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
        assert!(db
            .launch_window_admit("P:gr:fan", None, 3)
            .unwrap()
            .is_empty());
        assert!(db.launch_window_is_queued("c3").unwrap());
        assert!(db.launch_window_is_member("c3").unwrap());
        assert!(!db.launch_window_is_queued("c1").unwrap());

        // A hard terminal releases the slot and admits the oldest queued.
        assert_eq!(db.launch_window_release("c1", None, 4).unwrap(), vec!["c3"]);
        assert!(!db.launch_window_is_member("c1").unwrap());
        assert!(!db.launch_window_is_queued("c3").unwrap());

        // Releasing a non-member is a no-op.
        assert!(db
            .launch_window_release("nope", None, 5)
            .unwrap()
            .is_empty());
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
        assert!(db
            .launch_window_admit("Q:two", Some(1), 4)
            .unwrap()
            .is_empty());
        // The release under the same ceiling hands the slot across windows
        // only via that window's own admit — the sweep drives other keys.
        assert_eq!(
            db.launch_window_release("a1", Some(1), 5).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            db.launch_window_admit("Q:two", Some(1), 6).unwrap(),
            vec!["b1"]
        );
        assert_eq!(
            db.launch_window_keys_with_queue().unwrap(),
            Vec::<String>::new()
        );
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

    /// Established runtime state is exact-format authority. A stale owned
    /// database fails without mutation; there is no startup migration branch.
    #[test]
    fn open_rejects_old_owned_db_without_mutating_it() {
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

        assert!(RuntimeDb::open(&path).is_err());
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

        assert!(db
            .remove_seeded_continuation_runtime("T-next", "T-root", &initial)
            .unwrap());
        assert!(db.get_runtime_info("T-next").unwrap().is_none());
        assert!(!db
            .remove_seeded_continuation_runtime("T-next", "T-root", &initial)
            .unwrap());
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
        assert!(db
            .get_runtime_info("orphan-runtime-member")
            .unwrap()
            .is_none());
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
        assert!(db
            .inspect_chain_recovery_pins("c1", &["t1".to_string()])
            .unwrap()
            .is_empty());
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
    fn schema_version_mismatch_errors() {
        // O5: Schema version mismatch must surface as a typed error,
        // not silently degrade to None.
        let (_tmp, db) = fresh_db();
        db.insert_thread_runtime("t1", "c1").unwrap();
        let payload = serde_json::json!({ "schema_version": 999 }).to_string();
        db.conn
            .execute(
                "UPDATE thread_runtime SET pid = ?2, pgid = ?3, launch_metadata = ?4
                 WHERE thread_id = ?1",
                params!["t1", 1i64, 2i64, payload],
            )
            .unwrap();
        let err = db
            .get_runtime_info("t1")
            .expect_err("schema version mismatch must error");
        assert!(
            err.to_string().contains("schema_version mismatch"),
            "expected schema mismatch error, got: {err}"
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
        assert_eq!(claim.claimed_by, "daemon-a");
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
        assert_eq!(claim.claimed_by, "daemon-a");
        assert_eq!(claim.lease_expires_at_ms, i64::MAX);
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
        assert!(!db
            .mark_follow_child_terminal("chain-1", "tail-1", "completed", &envelope_1)
            .unwrap());
        assert_eq!(
            db.get_follow_waiter_by_key("fk-cohort")
                .unwrap()
                .unwrap()
                .phase,
            follow_phase::WAITING
        );

        let envelope_0 = serde_json::json!({"success": true, "result": 0});
        assert!(db
            .mark_follow_child_terminal("chain-0", "tail-0", "completed", &envelope_0)
            .unwrap());
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
        assert!(db
            .set_follow_child(
                "fk1",
                0,
                item_ref,
                &follow_child_spec_hash(item_ref, &BTreeMap::new(), &changed, None,).unwrap(),
                "child-1",
                "chain-1",
                &sealed,
            )
            .is_err());
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
        assert!(db
            .get_follow_waiter_by_parent_thread("nope")
            .unwrap()
            .is_none());
        assert!(db.get_follow_waiter_by_successor("nope").unwrap().is_none());

        // Cleared waiter is invisible to both accessors (terminal history moves
        // to the projection's continuation edge).
        db.clear_follow_waiter("fk1").unwrap();
        assert!(db
            .get_follow_waiter_by_parent_thread("parent-1")
            .unwrap()
            .is_none());
        assert!(db
            .get_follow_waiter_by_successor("succ-1")
            .unwrap()
            .is_none());
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
        assert!(db
            .mark_follow_child_terminal("chain-1", "c-tail", "completed", &env_a)
            .unwrap());
        // Same data again: idempotent no-op (no error, no rewrite).
        assert!(!db
            .mark_follow_child_terminal("chain-1", "c-tail", "completed", &env_a)
            .unwrap());
        // Conflicting terminal data is refused; the row keeps the first result.
        let env_b = serde_json::json!({"success": false, "result": "B"});
        assert!(db
            .mark_follow_child_terminal("chain-1", "c-other", "failed", &env_b)
            .is_err());
        let w = db.get_follow_waiter_by_key("fk1").unwrap().unwrap();
        assert_eq!(w.children[0].terminal_envelope, Some(env_a));
        assert_eq!(w.children[0].terminal_thread_id.as_deref(), Some("c-tail"));
        assert_eq!(w.children[0].terminal_status.as_deref(), Some("completed"));
    }
}
