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
        self.conn
            .query_row(
                "SELECT command_id, thread_id, command_type, status, requested_by, params,
                        result, created_at, claimed_at, completed_at
                 FROM thread_commands
                 WHERE command_id = ?1",
                params![command_id],
                read_command_row,
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("command missing from runtime db: {command_id}"))
    }
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
}
