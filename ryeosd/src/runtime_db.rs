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
"#;

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
        conn.execute_batch(SCHEMA_SQL)
            .context("failed to initialize runtime db schema")?;
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
        let lm_json = serde_json::to_string(launch_metadata)
            .context("failed to encode launch_metadata")?;
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
                        tracing::warn!(
                            thread_id = %thread_id,
                            persisted_schema_version = m.schema_version,
                            expected_schema_version = LAUNCH_METADATA_SCHEMA_VERSION,
                            payload_len = s.len(),
                            "launch_metadata schema_version mismatch; treating as None — \
                             resume eligibility and cancellation routing disabled \
                             for this thread until the row is rewritten"
                        );
                        None
                    } else {
                        Some(m)
                    }
                }
                Err(err) => {
                    // Do NOT log raw payload — ResumeContext.parameters
                    // can contain user/tool params that may include secrets.
                    tracing::warn!(
                        thread_id = %thread_id,
                        error = %err,
                        payload_len = s.len(),
                        "failed to decode launch_metadata; treating as None — \
                         resume eligibility and cancellation routing disabled \
                         for this thread until the row is rewritten"
                    );
                    None
                }
            },
        };
        Ok(Some(RuntimeInfo {
            pid,
            pgid,
            launch_metadata,
        }))
    }

    /// Read the auto-resume attempt counter for a thread. Missing row
    /// (or DB rows with no counter persisted) ⇒ 0.
    pub fn get_resume_attempts(&self, thread_id: &str) -> Result<u32> {
        let n: Option<i64> = self
            .conn
            .query_row(
                "SELECT resume_attempts FROM thread_runtime WHERE thread_id = ?1",
                params![thread_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(n.unwrap_or(0).max(0) as u32)
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
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Blob,
                Box::new(err),
            )
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
    fn garbage_launch_metadata_decodes_to_none_without_panic() {
        // Schema drift / corruption must surface as None (with a warn
        // log emitted) rather than panicking or silently dropping the
        // entire row.
        let (_tmp, db) = fresh_db();
        db.insert_thread_runtime("t1", "c1").unwrap();
        db.conn
            .execute(
                "UPDATE thread_runtime SET pid = ?2, pgid = ?3, launch_metadata = ?4
                 WHERE thread_id = ?1",
                params!["t1", 1i64, 2i64, "{not valid json"],
            )
            .unwrap();
        let info = db.get_runtime_info("t1").unwrap().unwrap();
        assert_eq!(info.pid, Some(1));
        assert!(info.launch_metadata.is_none());
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
    fn schema_version_mismatch_yields_none_with_warn() {
        let (_tmp, db) = fresh_db();
        db.insert_thread_runtime("t1", "c1").unwrap();
        // Persist a payload that decodes successfully but carries a
        // future schema_version. get_runtime_info must drop the
        // metadata to avoid acting on an unknown shape.
        let payload = serde_json::json!({ "schema_version": 999 }).to_string();
        db.conn
            .execute(
                "UPDATE thread_runtime SET pid = ?2, pgid = ?3, launch_metadata = ?4
                 WHERE thread_id = ?1",
                params!["t1", 1i64, 2i64, payload],
            )
            .unwrap();
        let info = db.get_runtime_info("t1").unwrap().unwrap();
        assert_eq!(info.pid, Some(1));
        assert!(info.launch_metadata.is_none());
    }
}
