use std::path::Path;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;

use crate::launch_metadata::{RuntimeLaunchMetadata, LAUNCH_METADATA_SCHEMA_VERSION};

#[derive(Debug, Clone, Default, Serialize)]
pub struct RuntimeInfo {
    pub pid: Option<i64>,
    pub pgid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch_metadata: Option<RuntimeLaunchMetadata>,
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
}

/// A durable parent↔child follow dependency. The graph checkpoint owns the
/// parent's cursor; THIS row owns the child/successor identities and the stored
/// child terminal envelope. Keyed by `follow_key`
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
    pub child_thread_id: Option<String>,
    pub child_chain_root_id: Option<String>,
    pub child_terminal_thread_id: Option<String>,
    pub child_terminal_status: Option<String>,
    /// Opaque canonical child terminal envelope (the supervisor's parsed
    /// `RuntimeResult`, or a synthesized failure envelope). Stored opaque here;
    /// the resume path classifies it via `classify_envelope`.
    pub terminal_envelope: Option<Value>,
    pub phase: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
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
    resume_attempts INTEGER NOT NULL DEFAULT 0
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
    child_thread_id TEXT,
    child_chain_root_id TEXT,
    child_terminal_thread_id TEXT,
    child_terminal_status TEXT,
    terminal_envelope TEXT,
    phase TEXT NOT NULL CHECK (phase IN ('reserved', 'waiting', 'ready', 'resuming')),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_follow_waiter_successor
    ON follow_waiter(parent_successor_thread_id);

CREATE UNIQUE INDEX IF NOT EXISTS idx_follow_waiter_child_chain
    ON follow_waiter(child_chain_root_id);

CREATE TABLE IF NOT EXISTS thread_child_link (
    child_thread_id TEXT PRIMARY KEY,
    parent_thread_id TEXT NOT NULL,
    relation TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_thread_child_link_parent
    ON thread_child_link(parent_thread_id);
"#;

use ryeos_state::sqlite_schema;

/// Application ID stamp for runtime.db.
/// RYEA = 0x5259_4541 ("RY" + "EA" for "runtime").
const RUNTIME_APP_ID: i32 = 0x5259_4541;

/// Schema spec for runtime.db — the single source of truth for
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
                    sqlite_schema::ColumnSpec { name: "follow_key", col_type: "TEXT", pk: true, not_null: true },
                    sqlite_schema::ColumnSpec { name: "parent_thread_id", col_type: "TEXT", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "parent_chain_root_id", col_type: "TEXT", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "parent_successor_thread_id", col_type: "TEXT", pk: false, not_null: false },
                    sqlite_schema::ColumnSpec { name: "follow_node", col_type: "TEXT", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "graph_run_id", col_type: "TEXT", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "step_count", col_type: "INTEGER", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "frontier_id", col_type: "TEXT", pk: false, not_null: false },
                    sqlite_schema::ColumnSpec { name: "child_thread_id", col_type: "TEXT", pk: false, not_null: false },
                    sqlite_schema::ColumnSpec { name: "child_chain_root_id", col_type: "TEXT", pk: false, not_null: false },
                    sqlite_schema::ColumnSpec { name: "child_terminal_thread_id", col_type: "TEXT", pk: false, not_null: false },
                    sqlite_schema::ColumnSpec { name: "child_terminal_status", col_type: "TEXT", pk: false, not_null: false },
                    sqlite_schema::ColumnSpec { name: "terminal_envelope", col_type: "TEXT", pk: false, not_null: false },
                    sqlite_schema::ColumnSpec { name: "phase", col_type: "TEXT", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "created_at_ms", col_type: "INTEGER", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "updated_at_ms", col_type: "INTEGER", pk: false, not_null: true },
                ],
            },
            sqlite_schema::TableSpec {
                name: "thread_child_link",
                columns: &[
                    sqlite_schema::ColumnSpec { name: "child_thread_id", col_type: "TEXT", pk: true, not_null: true },
                    sqlite_schema::ColumnSpec { name: "parent_thread_id", col_type: "TEXT", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "relation", col_type: "TEXT", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "created_at_ms", col_type: "INTEGER", pk: false, not_null: true },
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
                name: "idx_follow_waiter_successor",
                table: "follow_waiter",
                columns: &["parent_successor_thread_id"],
                unique: true,
            },
            sqlite_schema::IndexSpec {
                name: "idx_follow_waiter_child_chain",
                table: "follow_waiter",
                columns: &["child_chain_root_id"],
                unique: true,
            },
            sqlite_schema::IndexSpec {
                name: "idx_thread_child_link_parent",
                table: "thread_child_link",
                columns: &["parent_thread_id"],
                unique: false,
            },
        ],
    }
}

/// Forward-migrate an already-owned runtime.db to the current schema.
///
/// `SCHEMA_SQL` is entirely `CREATE ... IF NOT EXISTS`, so re-running it is
/// idempotent: it adds any newly-introduced table/index and no-ops on what
/// already exists, reconciling purely ADDITIVE schema growth. Non-additive
/// drift (a changed column) is intentionally NOT papered over here — the
/// `assert_owned` that runs next fails loud, forcing a real migration to be
/// written (cf. the scheduler DB's `rebuild_*` precedent).
fn migrate_owned_runtime_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL)
        .context("failed to apply additive runtime.db schema migration")?;
    Ok(())
}

pub struct RuntimeDb {
    conn: Connection,
}

impl RuntimeDb {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create runtime db dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open runtime db {}", path.display()))?;

        let spec = runtime_schema_spec();
        sqlite_schema::prepare_owned(&conn, &spec, SCHEMA_SQL, path, migrate_owned_runtime_db)?;
        Ok(Self { conn })
    }

    pub fn insert_thread_runtime(&self, thread_id: &str, chain_root_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO thread_runtime (thread_id, chain_root_id, pid, pgid, metadata, launch_metadata)
             VALUES (?1, ?2, NULL, NULL, NULL, NULL)",
            params![thread_id, chain_root_id],
        )?;
        Ok(())
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
        launch_metadata: &RuntimeLaunchMetadata,
    ) -> Result<()> {
        // Preserve seeded launch metadata. A self-attach over UDS sends only
        // thread/pid, so its `launch_metadata` is the serde default (empty); do
        // NOT let that clobber metadata already seeded on the row at spawn
        // (resume context / continuation spec). Update only pid/pgid in that case.
        if launch_metadata.is_empty() {
            let updated = self.conn.execute(
                "UPDATE thread_runtime SET pid = ?2, pgid = ?3 WHERE thread_id = ?1",
                params![thread_id, pid, pgid],
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
                SET pid = ?2, pgid = ?3, launch_metadata = ?4
              WHERE thread_id = ?1",
            params![thread_id, pid, pgid, lm_json],
        )?;
        if updated == 0 {
            bail!("thread_runtime row missing for thread_id: {thread_id}");
        }
        Ok(())
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
                "SELECT pid, pgid, launch_metadata FROM thread_runtime WHERE thread_id = ?1",
                params![thread_id],
                |row| {
                    let pid: Option<i64> = row.get(0)?;
                    let pgid: Option<i64> = row.get(1)?;
                    let lm_text: Option<String> = row.get(2)?;
                    Ok((pid, pgid, lm_text))
                },
            )
            .optional()?;
        let Some((pid, pgid, lm_text)) = raw else {
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
        Ok(Some(RuntimeInfo {
            pid,
            pgid,
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
    /// claim; a thread already mid-launch (a live, unexpired claim) returns
    /// [`LaunchClaimOutcome::AlreadyClaimed`]. A **stale** claim — one whose
    /// lease has expired, meaning the prior launcher died mid-launch (e.g. a
    /// daemon crash between create and spawn) — is reclaimed so the successor is
    /// not stranded. Lease expiry is the liveness proxy: a crashed daemon cannot
    /// renew, and a different daemon instance only reclaims after expiry.
    ///
    /// The whole decision is one `INSERT … ON CONFLICT DO UPDATE … WHERE expired`
    /// statement, so it is atomic against a concurrent claimer with no
    /// read-then-write race. `lease_ms` bounds how long this claim blocks a
    /// reclaim if the caller dies before releasing.
    pub fn claim_thread_launch(
        &self,
        thread_id: &str,
        claim_id: &str,
        claimed_by: &str,
        lease_ms: i64,
    ) -> Result<LaunchClaimOutcome> {
        let now_ms = lillux::time::timestamp_millis();
        let expires_ms = now_ms.saturating_add(lease_ms);
        // Insert if absent; on conflict, reclaim ONLY when the existing lease has
        // already expired (`lease_expires_at_ms <= now`). A live claim leaves the
        // row untouched → 0 rows changed → AlreadyClaimed.
        let changed = self.conn.execute(
            "INSERT INTO thread_launch_claim
                 (thread_id, claim_id, claimed_at_ms, lease_expires_at_ms, claimed_by)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(thread_id) DO UPDATE SET
                 claim_id = excluded.claim_id,
                 claimed_at_ms = excluded.claimed_at_ms,
                 lease_expires_at_ms = excluded.lease_expires_at_ms,
                 claimed_by = excluded.claimed_by
             WHERE thread_launch_claim.lease_expires_at_ms <= excluded.claimed_at_ms",
            params![thread_id, claim_id, now_ms, expires_ms, claimed_by],
        )?;
        Ok(if changed == 1 {
            LaunchClaimOutcome::Claimed
        } else {
            LaunchClaimOutcome::AlreadyClaimed
        })
    }

    /// Release a launch claim the caller owns (matched by `claim_id`), e.g. when
    /// the launch failed and the thread should become reclaimable immediately
    /// rather than waiting out the lease. Returns true if a row was removed.
    /// A mismatched `claim_id` (another launcher reclaimed in the meantime) is a
    /// no-op, never a cross-owner delete.
    pub fn release_thread_launch_claim(&self, thread_id: &str, claim_id: &str) -> Result<bool> {
        let removed = self.conn.execute(
            "DELETE FROM thread_launch_claim WHERE thread_id = ?1 AND claim_id = ?2",
            params![thread_id, claim_id],
        )?;
        Ok(removed > 0)
    }

    /// Delete ALL launch claims. Called once at daemon startup (before reconcile
    /// dispatches): a restart proves every prior in-process launcher is gone, so
    /// any surviving claim is stale and would otherwise block a reconcile relaunch
    /// of a `created` successor until the lease expired. Returns the count removed.
    pub fn clear_all_launch_claims(&self) -> Result<usize> {
        Ok(self
            .conn
            .execute("DELETE FROM thread_launch_claim", [])?)
    }

    /// Read the current launch claim for a thread, if any. The reconciler uses
    /// this to tell an unlaunched successor (no claim, or expired) from one
    /// mid-launch (live claim) without attempting to claim.
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
        let now = now_rfc3339();
        self.conn.execute(
            "INSERT INTO thread_commands (
                thread_id, command_type, status, requested_by, params, result,
                created_at, claimed_at, completed_at
             ) VALUES (?1, ?2, 'pending', ?3, ?4, NULL, ?5, NULL, NULL)",
            params![
                &cmd.thread_id,
                &cmd.command_type,
                &cmd.requested_by,
                json_blob(&cmd.params)?,
                now,
            ],
        )?;

        let command_id = self.conn.last_insert_rowid();
        self.load_command(command_id)
    }

    pub fn claim_commands(&self, thread_id: &str) -> Result<Vec<CommandRecord>> {
        let now = now_rfc3339();

        let mut stmt = self.conn.prepare(
            "SELECT command_id, thread_id, command_type, status, requested_by, params,
                    result, created_at, claimed_at, completed_at
             FROM thread_commands
             WHERE thread_id = ?1 AND status = 'pending'
             ORDER BY command_id ASC",
        )?;
        let rows = stmt.query_map(params![thread_id], read_command_row)?;

        let mut commands = Vec::new();
        for row in rows {
            let mut command = row?;
            self.conn.execute(
                "UPDATE thread_commands SET status = 'claimed', claimed_at = ?2 WHERE command_id = ?1",
                params![command.command_id, now],
            )?;
            command.status = "claimed".to_string();
            command.claimed_at = Some(now.clone());
            commands.push(command);
        }

        drop(stmt);
        Ok(commands)
    }

    pub fn complete_command(
        &self,
        command_id: i64,
        status: &str,
        result: Option<&Value>,
    ) -> Result<CommandRecord> {
        let updated = self.conn.execute(
            "UPDATE thread_commands
             SET status = ?2,
                 result = ?3,
                 completed_at = ?4
             WHERE command_id = ?1 AND status IN ('pending', 'claimed')",
            params![command_id, status, json_blob_ref(result)?, now_rfc3339()],
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
        let open: Vec<(i64, String)> = {
            let mut stmt = self.conn.prepare(
                "SELECT command_id, command_type FROM thread_commands
                 WHERE thread_id = ?1 AND status IN ('pending', 'claimed')
                 ORDER BY command_id ASC",
            )?;
            let rows = stmt
                .query_map(params![thread_id], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        let now = now_rfc3339();
        let mut settled = Vec::with_capacity(open.len());
        for (id, command_type) in open {
            let fulfilled = command_fulfilled_by_terminal(&command_type, terminal_status);
            let status = if fulfilled { "completed" } else { "rejected" };
            let result = serde_json::json!({
                "reason": if fulfilled {
                    format!("thread settled {terminal_status}, fulfilling the {command_type} command")
                } else {
                    format!("thread finalized ({terminal_status}) before the {command_type} command was handled")
                }
            });
            let updated = self.conn.execute(
                "UPDATE thread_commands SET status = ?2, result = ?3, completed_at = ?4
                 WHERE command_id = ?1 AND status IN ('pending', 'claimed')",
                params![id, status, json_blob_ref(Some(&result))?, now],
            )?;
            if updated > 0 {
                if let Some(record) = self.get_command(id)? {
                    settled.push(record);
                }
            }
        }
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
        Ok(self
            .conn
            .query_row(
                "SELECT command_id, thread_id, command_type, status, requested_by, params,
                        result, created_at, claimed_at, completed_at
                 FROM thread_commands
                 WHERE command_id = ?1",
                params![command_id],
                read_command_row,
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
        let now = lillux::time::timestamp_millis();
        self.conn.execute(
            "INSERT INTO follow_waiter (
                 follow_key, parent_thread_id, parent_chain_root_id,
                 follow_node, graph_run_id, step_count, frontier_id,
                 phase, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'reserved', ?8, ?8)
             ON CONFLICT(follow_key) DO NOTHING",
            params![
                seed.follow_key,
                seed.parent_thread_id,
                seed.parent_chain_root_id,
                seed.follow_node,
                seed.graph_run_id,
                seed.step_count,
                seed.frontier_id,
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
        child_thread_id: &str,
        child_chain_root_id: &str,
    ) -> Result<()> {
        let w = self.require_follow_waiter(follow_key)?;
        match (w.child_thread_id.as_deref(), w.child_chain_root_id.as_deref()) {
            (None, None) => {}
            (Some(t), Some(c)) if t == child_thread_id && c == child_chain_root_id => {
                return Ok(())
            }
            _ => bail!(
                "follow waiter {follow_key} already has a different child; refusing to overwrite"
            ),
        }
        self.conn.execute(
            "UPDATE follow_waiter
                SET child_thread_id = ?2, child_chain_root_id = ?3, updated_at_ms = ?4
              WHERE follow_key = ?1",
            params![
                follow_key,
                child_thread_id,
                child_chain_root_id,
                lillux::time::timestamp_millis()
            ],
        )?;
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
            params![follow_key, successor_thread_id, lillux::time::timestamp_millis()],
        )?;
        Ok(())
    }

    /// Transition → waiting. Only `reserved → waiting` (idempotent on
    /// `waiting`); requires the child + successor recorded first. Never regresses
    /// a later phase.
    pub fn mark_follow_waiting(&self, follow_key: &str) -> Result<()> {
        let w = self.require_follow_waiter(follow_key)?;
        if w.phase == follow_phase::WAITING {
            return Ok(());
        }
        if w.phase != follow_phase::RESERVED {
            bail!("follow waiter {follow_key} cannot transition {} -> waiting", w.phase);
        }
        if w.child_chain_root_id.is_none() || w.parent_successor_thread_id.is_none() {
            bail!(
                "follow waiter {follow_key} cannot mark waiting before child + successor are recorded"
            );
        }
        self.set_follow_phase_unchecked(follow_key, follow_phase::WAITING)
    }

    /// Transition → resuming. Only `ready → resuming` (idempotent on
    /// `resuming`); requires the terminal envelope + successor present.
    pub fn mark_follow_resuming(&self, follow_key: &str) -> Result<()> {
        let w = self.require_follow_waiter(follow_key)?;
        if w.phase == follow_phase::RESUMING {
            return Ok(());
        }
        if w.phase != follow_phase::READY {
            bail!("follow waiter {follow_key} cannot transition {} -> resuming", w.phase);
        }
        if w.terminal_envelope.is_none() || w.parent_successor_thread_id.is_none() {
            bail!(
                "follow waiter {follow_key} cannot resume without terminal envelope + successor"
            );
        }
        self.set_follow_phase_unchecked(follow_key, follow_phase::RESUMING)
    }

    fn require_follow_waiter(&self, follow_key: &str) -> Result<FollowWaiter> {
        self.get_follow_waiter_by_key(follow_key)?.ok_or_else(|| {
            anyhow::anyhow!("follow waiter row missing for follow_key: {follow_key}")
        })
    }

    fn set_follow_phase_unchecked(&self, follow_key: &str, phase: &str) -> Result<()> {
        let updated = self.conn.execute(
            "UPDATE follow_waiter SET phase = ?2, updated_at_ms = ?3 WHERE follow_key = ?1",
            params![follow_key, phase, lillux::time::timestamp_millis()],
        )?;
        if updated == 0 {
            bail!("follow waiter row missing for follow_key: {follow_key}");
        }
        Ok(())
    }

    /// Mark the followed child chain terminal, keyed by the child's chain root.
    /// Stores the canonical terminal envelope and flips the waiter to `ready`.
    ///
    /// Idempotent and immutable once captured. The only state that transitions
    /// is `waiting` (where the child + successor are recorded); a row already
    /// `ready` keeps its first terminal result (a duplicate with identical data
    /// is a no-op, conflicting data is rejected), `resuming` is never downgraded,
    /// and `reserved` (child not yet launched) is not eligible. Returns `true`
    /// only on the first `waiting → ready` transition.
    pub fn mark_follow_child_terminal(
        &self,
        child_chain_root_id: &str,
        child_terminal_thread_id: &str,
        child_terminal_status: &str,
        terminal_envelope: &Value,
    ) -> Result<bool> {
        let Some(w) = self.get_follow_waiter_by_child_chain(child_chain_root_id)? else {
            return Ok(false);
        };
        match w.phase.as_str() {
            p if p == follow_phase::READY => {
                let same = w.child_terminal_thread_id.as_deref() == Some(child_terminal_thread_id)
                    && w.child_terminal_status.as_deref() == Some(child_terminal_status)
                    && w.terminal_envelope.as_ref() == Some(terminal_envelope);
                if same {
                    Ok(false)
                } else {
                    bail!(
                        "follow waiter for child chain {child_chain_root_id} is already ready \
                         with a different terminal result; refusing to overwrite"
                    );
                }
            }
            p if p == follow_phase::WAITING => {
                let envelope_json = serde_json::to_string(terminal_envelope)
                    .context("failed to encode follow terminal envelope")?;
                self.conn.execute(
                    "UPDATE follow_waiter
                        SET phase = 'ready',
                            child_terminal_thread_id = ?2,
                            child_terminal_status = ?3,
                            terminal_envelope = ?4,
                            updated_at_ms = ?5
                      WHERE follow_key = ?1",
                    params![
                        w.follow_key,
                        child_terminal_thread_id,
                        child_terminal_status,
                        envelope_json,
                        lillux::time::timestamp_millis(),
                    ],
                )?;
                Ok(true)
            }
            // resuming (resume in progress) or reserved (child not yet launched).
            _ => Ok(false),
        }
    }

    pub fn get_follow_waiter_by_key(&self, follow_key: &str) -> Result<Option<FollowWaiter>> {
        self.conn
            .query_row(
                &format!("SELECT {FOLLOW_WAITER_COLUMNS} FROM follow_waiter WHERE follow_key = ?1"),
                params![follow_key],
                read_follow_waiter_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn get_follow_waiter_by_child_chain(
        &self,
        child_chain_root_id: &str,
    ) -> Result<Option<FollowWaiter>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {FOLLOW_WAITER_COLUMNS} FROM follow_waiter \
                     WHERE child_chain_root_id = ?1"
                ),
                params![child_chain_root_id],
                read_follow_waiter_row,
            )
            .optional()
            .map_err(Into::into)
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
            .optional()
            .map_err(Into::into)
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
            .optional()
            .map_err(Into::into)
    }

    /// All active follow waiters. The table holds only non-cleared rows, so
    /// every row here is recoverable by reconcile.
    pub fn list_follow_waiters(&self) -> Result<Vec<FollowWaiter>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {FOLLOW_WAITER_COLUMNS} FROM follow_waiter ORDER BY created_at_ms ASC"
        ))?;
        let rows = stmt.query_map([], read_follow_waiter_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// Delete a follow waiter — only once the parent successor is independently
    /// recoverable (checkpoint copied with the result + launch claimed, or the
    /// successor reached terminal).
    pub fn clear_follow_waiter(&self, follow_key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM follow_waiter WHERE follow_key = ?1", params![follow_key])?;
        Ok(())
    }
}

const FOLLOW_WAITER_COLUMNS: &str = "follow_key, parent_thread_id, parent_chain_root_id, \
     parent_successor_thread_id, follow_node, graph_run_id, step_count, frontier_id, \
     child_thread_id, child_chain_root_id, child_terminal_thread_id, child_terminal_status, \
     terminal_envelope, phase, created_at_ms, updated_at_ms";

fn read_follow_waiter_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FollowWaiter> {
    let terminal_envelope_json: Option<String> = row.get(12)?;
    // Corrupt persisted JSON is a hard read error, not a silent `None` (which
    // would make a `ready`/`resuming` row look incomplete and strand the parent).
    let terminal_envelope = match terminal_envelope_json {
        Some(s) => Some(serde_json::from_str(&s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(12, rusqlite::types::Type::Text, Box::new(e))
        })?),
        None => None,
    };
    Ok(FollowWaiter {
        follow_key: row.get(0)?,
        parent_thread_id: row.get(1)?,
        parent_chain_root_id: row.get(2)?,
        parent_successor_thread_id: row.get(3)?,
        follow_node: row.get(4)?,
        graph_run_id: row.get(5)?,
        step_count: row.get(6)?,
        frontier_id: row.get(7)?,
        child_thread_id: row.get(8)?,
        child_chain_root_id: row.get(9)?,
        child_terminal_thread_id: row.get(10)?,
        child_terminal_status: row.get(11)?,
        terminal_envelope,
        phase: row.get(13)?,
        created_at_ms: row.get(14)?,
        updated_at_ms: row.get(15)?,
    })
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

    #[test]
    fn attach_and_read_launch_metadata_roundtrip() {
        let (_tmp, db) = fresh_db();
        db.insert_thread_runtime("t1", "c1").unwrap();
        let lm = RuntimeLaunchMetadata {
            cancellation_mode: Some(CancellationMode::Graceful { grace_secs: 9 }),
            ..Default::default()
        };
        db.attach_process("t1", 1234, 5678, &lm).unwrap();

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
        db.claim_commands("t1").unwrap();

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
        assert!(db.settle_open_commands("t1", "cancelled").unwrap().is_empty());
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
        db.attach_process("t1", 1234, 5678, &seeded).unwrap();

        // Self-attach with default (empty) metadata, new pid/pgid.
        db.attach_process("t1", 4321, 8765, &RuntimeLaunchMetadata::default())
            .unwrap();

        let info = db.get_runtime_info("t1").unwrap().unwrap();
        assert_eq!(info.pid, Some(4321), "pid still updated");
        assert_eq!(info.pgid, Some(8765), "pgid still updated");
        assert_eq!(
            info.launch_metadata
                .expect("seeded metadata preserved")
                .cancellation_mode,
            seeded.cancellation_mode,
            "empty attach must not clobber seeded metadata"
        );
    }

    #[test]
    fn attach_with_hard_cancellation_roundtrip() {
        let (_tmp, db) = fresh_db();
        db.insert_thread_runtime("t1", "c1").unwrap();
        let lm = RuntimeLaunchMetadata {
            cancellation_mode: Some(CancellationMode::Hard),
            ..Default::default()
        };
        db.attach_process("t1", 1, 2, &lm).unwrap();
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

    /// An owned runtime.db stamped by an earlier daemon that predates the
    /// `thread_launch_claim` table must start cleanly: the open-time additive
    /// migration creates the missing table rather than bailing on it.
    #[test]
    fn open_migrates_old_owned_db_missing_launch_claim() {
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
            // Seed a runtime row so we also prove the migration preserves data.
            conn.execute(
                "INSERT INTO thread_runtime (thread_id, chain_root_id) VALUES (?1, ?2)",
                params!["t-old", "c-old"],
            )
            .unwrap();
        }

        // Open must succeed (no "missing expected table" bail)…
        let db = RuntimeDb::open(&path).unwrap();
        // …the new table is now usable…
        assert_eq!(
            db.claim_thread_launch("t-old", "claim-1", "launcher", 60_000)
                .unwrap(),
            LaunchClaimOutcome::Claimed
        );
        // …and pre-existing runtime state survived the migration.
        assert!(db.get_runtime_info("t-old").unwrap().is_some());
    }

    #[test]
    fn null_launch_metadata_yields_none() {
        let (_tmp, db) = fresh_db();
        db.insert_thread_runtime("t1", "c1").unwrap();
        db.conn
            .execute(
                "UPDATE thread_runtime SET pid = ?2, pgid = ?3 WHERE thread_id = ?1",
                params!["t1", 7i64, 8i64],
            )
            .unwrap();
        let info = db.get_runtime_info("t1").unwrap().unwrap();
        assert_eq!(info.pid, Some(7));
        assert_eq!(info.pgid, Some(8));
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
            .attach_process("missing", 1, 2, &lm)
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
        // Fresh thread: first claim with a long lease wins.
        assert_eq!(
            db.claim_thread_launch("t1", "c1", "daemon-a", 60_000)
                .unwrap(),
            LaunchClaimOutcome::Claimed
        );
        // A second launcher cannot claim while the lease is live.
        assert_eq!(
            db.claim_thread_launch("t1", "c2", "daemon-b", 60_000)
                .unwrap(),
            LaunchClaimOutcome::AlreadyClaimed
        );
        // The live claim still belongs to the first caller.
        let claim = db.get_launch_claim("t1").unwrap().expect("claim present");
        assert_eq!(claim.claim_id, "c1");
        assert_eq!(claim.claimed_by, "daemon-a");
    }

    #[test]
    fn launch_claim_stale_lease_is_reclaimed() {
        let (_tmp, db) = fresh_db();
        // A claim whose lease is already in the past (prior launcher died
        // mid-launch) must be reclaimable.
        assert_eq!(
            db.claim_thread_launch("t1", "c1", "daemon-a", -1_000)
                .unwrap(),
            LaunchClaimOutcome::Claimed
        );
        assert_eq!(
            db.claim_thread_launch("t1", "c2", "daemon-b", 60_000)
                .unwrap(),
            LaunchClaimOutcome::Claimed,
            "expired lease must be reclaimed by a new launcher"
        );
        let claim = db.get_launch_claim("t1").unwrap().expect("claim present");
        assert_eq!(claim.claim_id, "c2", "reclaim overwrites the owner");
        assert_eq!(claim.claimed_by, "daemon-b");
    }

    #[test]
    fn launch_claim_release_frees_for_reclaim() {
        let (_tmp, db) = fresh_db();
        assert_eq!(
            db.claim_thread_launch("t1", "c1", "daemon-a", 60_000)
                .unwrap(),
            LaunchClaimOutcome::Claimed
        );
        // A mismatched claim_id must not delete another owner's claim.
        assert!(!db.release_thread_launch_claim("t1", "other").unwrap());
        assert!(db.get_launch_claim("t1").unwrap().is_some());
        // The owner releases; the thread becomes immediately reclaimable.
        assert!(db.release_thread_launch_claim("t1", "c1").unwrap());
        assert!(db.get_launch_claim("t1").unwrap().is_none());
        assert_eq!(
            db.claim_thread_launch("t1", "c2", "daemon-b", 60_000)
                .unwrap(),
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
        }
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
        db.set_follow_child("fk1", "child-1", "chain-child").unwrap();
        db.set_follow_parent_successor("fk1", "succ-1").unwrap();
        db.mark_follow_waiting("fk1").unwrap();

        let w = db.get_follow_waiter_by_key("fk1").unwrap().unwrap();
        assert_eq!(w.phase, follow_phase::WAITING);
        assert_eq!(w.child_chain_root_id.as_deref(), Some("chain-child"));
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
        assert_eq!(ready.child_terminal_status.as_deref(), Some("completed"));
        assert_eq!(ready.terminal_envelope, Some(envelope));

        db.clear_follow_waiter("fk1").unwrap();
        assert!(db.get_follow_waiter_by_key("fk1").unwrap().is_none());
        assert!(db.list_follow_waiters().unwrap().is_empty());
    }

    #[test]
    fn lookup_by_parent_and_successor_thread() {
        let (_tmp, db) = fresh_db();
        db.reserve_follow(&seed_follow("fk1")).unwrap();
        db.set_follow_child("fk1", "child-1", "chain-child").unwrap();
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
        assert!(db.get_follow_waiter_by_parent_thread("nope").unwrap().is_none());
        assert!(db.get_follow_waiter_by_successor("nope").unwrap().is_none());

        // Cleared waiter is invisible to both accessors (terminal history moves
        // to the projection's continuation edge).
        db.clear_follow_waiter("fk1").unwrap();
        assert!(db.get_follow_waiter_by_parent_thread("parent-1").unwrap().is_none());
        assert!(db.get_follow_waiter_by_successor("succ-1").unwrap().is_none());
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
        db.set_follow_child("fk1", "child-1", "shared-chain").unwrap();
        // A second follow cannot claim the same child chain root (UNIQUE).
        assert!(
            db.set_follow_child("fk2", "child-2", "shared-chain").is_err(),
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
        db.set_follow_child("fk1", "child-1", "chain-1").unwrap();
        db.set_follow_child("fk1", "child-1", "chain-1").unwrap(); // idempotent
        assert!(db.set_follow_child("fk1", "child-2", "chain-2").is_err());
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
        db.set_follow_child("fk1", "c", "chain-1").unwrap();
        db.set_follow_parent_successor("fk1", "succ-1").unwrap();
        db.mark_follow_waiting("fk1").unwrap();
        // Cannot resume from waiting (must be ready first).
        assert!(db.mark_follow_resuming("fk1").is_err());
        db.mark_follow_child_terminal("chain-1", "c-tail", "completed", &serde_json::json!({"ok": true}))
            .unwrap();
        db.mark_follow_resuming("fk1").unwrap();
        // A late/duplicate terminal hook must NOT downgrade resuming → ready.
        let matched = db
            .mark_follow_child_terminal("chain-1", "c-tail", "completed", &serde_json::json!({"ok": true}))
            .unwrap();
        assert!(!matched, "resuming row must not be downgraded by a late terminal");
        assert_eq!(
            db.get_follow_waiter_by_key("fk1").unwrap().unwrap().phase,
            follow_phase::RESUMING
        );
    }

    #[test]
    fn corrupt_terminal_envelope_json_fails_read() {
        let (_tmp, db) = fresh_db();
        db.reserve_follow(&seed_follow("fk1")).unwrap();
        db.conn
            .execute(
                "UPDATE follow_waiter SET terminal_envelope = '{not json' WHERE follow_key = 'fk1'",
                [],
            )
            .unwrap();
        assert!(db.get_follow_waiter_by_key("fk1").is_err());
    }

    #[test]
    fn ready_terminal_result_is_immutable() {
        let (_tmp, db) = fresh_db();
        db.reserve_follow(&seed_follow("fk1")).unwrap();
        db.set_follow_child("fk1", "c", "chain-1").unwrap();
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
        assert_eq!(w.terminal_envelope, Some(env_a));
        assert_eq!(w.child_terminal_thread_id.as_deref(), Some("c-tail"));
        assert_eq!(w.child_terminal_status.as_deref(), Some("completed"));
    }
}
