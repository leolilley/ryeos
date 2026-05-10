//! Scheduler SQLite database — `scheduler.sqlite3`.
//!
//! Separate from `projection.sqlite3` (which has strict schema validation).
//! Own `application_id`, own schema, own rebuild path.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use super::types::{FireRecord, ScheduleSpecRecord};

// ── Schema ──────────────────────────────────────────────────────────

const SCHEMA_SQL: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;

CREATE TABLE IF NOT EXISTS schedule_specs (
    schedule_id        TEXT PRIMARY KEY,
    item_ref           TEXT NOT NULL,
    params             TEXT NOT NULL DEFAULT '{}',
    schedule_type      TEXT NOT NULL,
    expression         TEXT NOT NULL,
    timezone           TEXT NOT NULL DEFAULT 'UTC',
    misfire_policy     TEXT NOT NULL DEFAULT 'skip',
    overlap_policy     TEXT NOT NULL DEFAULT 'skip',
    enabled            INTEGER NOT NULL DEFAULT 1,
    project_root       TEXT,
    signer_fingerprint TEXT NOT NULL,
    spec_hash          TEXT NOT NULL,
    last_modified      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS schedule_fires (
    fire_id            TEXT PRIMARY KEY,
    schedule_id        TEXT NOT NULL,
    scheduled_at       INTEGER NOT NULL,
    fired_at           INTEGER,
    completed_at       INTEGER,
    thread_id          TEXT,
    status             TEXT NOT NULL,
    trigger_reason     TEXT NOT NULL DEFAULT 'normal',
    outcome            TEXT,
    signer_fingerprint TEXT
);

CREATE INDEX IF NOT EXISTS idx_fires_schedule_id
    ON schedule_fires(schedule_id);
CREATE INDEX IF NOT EXISTS idx_fires_status
    ON schedule_fires(status);
CREATE INDEX IF NOT EXISTS idx_fires_schedule_scheduled
    ON schedule_fires(schedule_id, scheduled_at DESC);
"#;

/// RYSC = 0x5259_5343 ("RY" + "SC" for scheduler)
const SCHEDULER_APP_ID: i32 = 0x5259_5343;

use ryeos_state::sqlite_schema;

fn scheduler_schema_spec() -> sqlite_schema::SchemaSpec {
    sqlite_schema::SchemaSpec {
        application_id: SCHEDULER_APP_ID,
        tables: &[
            sqlite_schema::TableSpec {
                name: "schedule_specs",
                columns: &[
                    sqlite_schema::ColumnSpec { name: "schedule_id", col_type: "TEXT", pk: true, not_null: true },
                    sqlite_schema::ColumnSpec { name: "item_ref", col_type: "TEXT", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "params", col_type: "TEXT", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "schedule_type", col_type: "TEXT", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "expression", col_type: "TEXT", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "timezone", col_type: "TEXT", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "misfire_policy", col_type: "TEXT", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "overlap_policy", col_type: "TEXT", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "enabled", col_type: "INTEGER", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "project_root", col_type: "TEXT", pk: false, not_null: false },
                    sqlite_schema::ColumnSpec { name: "signer_fingerprint", col_type: "TEXT", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "spec_hash", col_type: "TEXT", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "last_modified", col_type: "INTEGER", pk: false, not_null: true },
                ],
            },
            sqlite_schema::TableSpec {
                name: "schedule_fires",
                columns: &[
                    sqlite_schema::ColumnSpec { name: "fire_id", col_type: "TEXT", pk: true, not_null: true },
                    sqlite_schema::ColumnSpec { name: "schedule_id", col_type: "TEXT", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "scheduled_at", col_type: "INTEGER", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "fired_at", col_type: "INTEGER", pk: false, not_null: false },
                    sqlite_schema::ColumnSpec { name: "completed_at", col_type: "INTEGER", pk: false, not_null: false },
                    sqlite_schema::ColumnSpec { name: "thread_id", col_type: "TEXT", pk: false, not_null: false },
                    sqlite_schema::ColumnSpec { name: "status", col_type: "TEXT", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "trigger_reason", col_type: "TEXT", pk: false, not_null: true },
                    sqlite_schema::ColumnSpec { name: "outcome", col_type: "TEXT", pk: false, not_null: false },
                    sqlite_schema::ColumnSpec { name: "signer_fingerprint", col_type: "TEXT", pk: false, not_null: false },
                ],
            },
        ],
        indexes: &[
            sqlite_schema::IndexSpec { name: "idx_fires_schedule_id", table: "schedule_fires", columns: &["schedule_id"], unique: false },
            sqlite_schema::IndexSpec { name: "idx_fires_status", table: "schedule_fires", columns: &["status"], unique: false },
            sqlite_schema::IndexSpec { name: "idx_fires_schedule_scheduled", table: "schedule_fires", columns: &["schedule_id", "scheduled_at"], unique: false },
        ],
    }
}

// ── Database wrapper ────────────────────────────────────────────────

pub struct SchedulerDb {
    inner: std::sync::Mutex<Connection>,
}

impl SchedulerDb {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create scheduler db dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open scheduler db {}", path.display()))?;

        let spec = scheduler_schema_spec();
        if sqlite_schema::is_empty_or_owned(&conn, spec.application_id)? {
            sqlite_schema::init_owned(&conn, &spec, SCHEMA_SQL, path)?;
        } else {
            sqlite_schema::assert_owned(&conn, &spec, path)?;
        }
        Ok(Self { inner: std::sync::Mutex::new(conn) })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.inner.lock().map_err(|e| anyhow!("scheduler db lock poisoned: {}", e))
    }

    // ── schedule_specs ──────────────────────────────────────────

    pub fn upsert_spec(&self, rec: &ScheduleSpecRecord) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO schedule_specs
                (schedule_id, item_ref, params, schedule_type, expression,
                 timezone, misfire_policy, overlap_policy, enabled,
                 project_root, signer_fingerprint, spec_hash, last_modified)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
             ON CONFLICT(schedule_id) DO UPDATE SET
                item_ref=excluded.item_ref, params=excluded.params,
                schedule_type=excluded.schedule_type, expression=excluded.expression,
                timezone=excluded.timezone, misfire_policy=excluded.misfire_policy,
                overlap_policy=excluded.overlap_policy, enabled=excluded.enabled,
                project_root=excluded.project_root, signer_fingerprint=excluded.signer_fingerprint,
                spec_hash=excluded.spec_hash, last_modified=excluded.last_modified",
            params![
                rec.schedule_id, rec.item_ref, rec.params,
                rec.schedule_type, rec.expression, rec.timezone,
                rec.misfire_policy, rec.overlap_policy, rec.enabled as i32,
                rec.project_root, rec.signer_fingerprint, rec.spec_hash,
                rec.last_modified,
            ],
        )
        .with_context(|| format!("upsert_spec failed for {}", rec.schedule_id))?;
        Ok(())
    }

    pub fn delete_spec(&self, schedule_id: &str) -> Result<bool> {
        let n = self.lock()?.execute(
            "DELETE FROM schedule_specs WHERE schedule_id = ?1",
            params![schedule_id],
        )?;
        Ok(n > 0)
    }

    pub fn get_spec(&self, schedule_id: &str) -> Result<Option<ScheduleSpecRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT schedule_id, item_ref, params, schedule_type, expression,
                    timezone, misfire_policy, overlap_policy, enabled,
                    project_root, signer_fingerprint, spec_hash, last_modified
             FROM schedule_specs WHERE schedule_id = ?1",
        )?;
        stmt.query_row(params![schedule_id], |row| Ok(row_to_spec(row)))
            .optional()
            .map_err(Into::into)
    }

    pub fn load_enabled_specs(&self) -> Result<Vec<ScheduleSpecRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT schedule_id, item_ref, params, schedule_type, expression,
                    timezone, misfire_policy, overlap_policy, enabled,
                    project_root, signer_fingerprint, spec_hash, last_modified
             FROM schedule_specs WHERE enabled = 1",
        )?;
        let rows = stmt.query_map([], |row| Ok(row_to_spec(row)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn list_specs(&self, enabled_only: bool, schedule_type: Option<&str>) -> Result<Vec<ScheduleSpecRecord>> {
        let sql = match (enabled_only, schedule_type) {
            (true, Some(_)) => "SELECT schedule_id, item_ref, params, schedule_type, expression,
                                       timezone, misfire_policy, overlap_policy, enabled,
                                       project_root, signer_fingerprint, spec_hash, last_modified
                                FROM schedule_specs WHERE enabled = 1 AND schedule_type = ?",
            (true, None) => "SELECT schedule_id, item_ref, params, schedule_type, expression,
                                    timezone, misfire_policy, overlap_policy, enabled,
                                    project_root, signer_fingerprint, spec_hash, last_modified
                             FROM schedule_specs WHERE enabled = 1",
            (false, Some(_)) => "SELECT schedule_id, item_ref, params, schedule_type, expression,
                                        timezone, misfire_policy, overlap_policy, enabled,
                                        project_root, signer_fingerprint, spec_hash, last_modified
                                 FROM schedule_specs WHERE schedule_type = ?",
            (false, None) => "SELECT schedule_id, item_ref, params, schedule_type, expression,
                                    timezone, misfire_policy, overlap_policy, enabled,
                                    project_root, signer_fingerprint, spec_hash, last_modified
                             FROM schedule_specs",
        };
        let conn = self.lock()?;
        let mut stmt = conn.prepare(sql)?;
        let rows: Vec<ScheduleSpecRecord> = if let Some(st) = schedule_type {
            stmt.query_map(params![st], |row| Ok(row_to_spec(row)))?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            stmt.query_map([], |row| Ok(row_to_spec(row)))?
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(rows)
    }

    pub fn delete_stale_specs(&self, live_ids: &[&str]) -> Result<usize> {
        if live_ids.is_empty() {
            let n = self.lock()?.execute("DELETE FROM schedule_specs", [])?;
            return Ok(n);
        }
        let placeholders: Vec<String> = live_ids.iter().enumerate().map(|(i, _)| format!("?{}", i + 1)).collect();
        let sql = format!("DELETE FROM schedule_specs WHERE schedule_id NOT IN ({})", placeholders.join(","));
        let params: Vec<&dyn rusqlite::ToSql> = live_ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        let n = self.lock()?.execute(&sql, params.as_slice())?;
        Ok(n)
    }

    // ── schedule_fires ──────────────────────────────────────────

    pub fn upsert_fire(&self, rec: &FireRecord) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO schedule_fires
                (fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                 status, trigger_reason, outcome, signer_fingerprint)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(fire_id) DO UPDATE SET
                status=excluded.status,
                fired_at=CASE WHEN fired_at IS NULL THEN excluded.fired_at ELSE fired_at END,
                completed_at=COALESCE(excluded.completed_at, completed_at),
                thread_id=COALESCE(excluded.thread_id, thread_id),
                outcome=COALESCE(excluded.outcome, outcome),
                trigger_reason=excluded.trigger_reason,
                signer_fingerprint=COALESCE(excluded.signer_fingerprint, signer_fingerprint)",
            params![
                rec.fire_id, rec.schedule_id, rec.scheduled_at,
                rec.fired_at, rec.completed_at, rec.thread_id, rec.status,
                rec.trigger_reason, rec.outcome, rec.signer_fingerprint,
            ],
        )
        .with_context(|| format!("upsert_fire failed for {}", rec.fire_id))?;
        Ok(())
    }

    /// Atomic claim: INSERT if absent. Returns true if the insert succeeded
    /// (fire was claimed), false if it already existed.
    pub fn claim_fire(&self, rec: &FireRecord) -> Result<bool> {
        let changed = self.lock()?.execute(
            "INSERT OR IGNORE INTO schedule_fires
                (fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                 status, trigger_reason, outcome, signer_fingerprint)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                rec.fire_id, rec.schedule_id, rec.scheduled_at,
                rec.fired_at, rec.completed_at, rec.thread_id, rec.status,
                rec.trigger_reason, rec.outcome, rec.signer_fingerprint,
            ],
        )
        .with_context(|| format!("claim_fire failed for {}", rec.fire_id))?;
        Ok(changed > 0)
    }

    /// Reclaim a fire that was persisted but never got a running thread.
    /// Safe to redispatch if: status is 'dispatched' AND thread_id is NULL
    /// or the thread row doesn't exist in the runtime DB.
    /// Updates fired_at to now and returns true if reclaimable.
    pub fn reclaim_fire(&self, fire_id: &str) -> Result<bool> {
        let conn = self.lock()?;

        // Check if the fire exists and is in dispatched state with no thread
        let is_reclaimable: bool = conn.query_row(
            "SELECT status = 'dispatched' AND thread_id IS NULL
             FROM schedule_fires WHERE fire_id = ?1",
            params![fire_id],
            |row| row.get::<_, bool>(0),
        ).optional()?.unwrap_or(false);

        if !is_reclaimable {
            // Also check: dispatched with thread_id set, but thread may not exist.
            // For that case, we check if the fire exists at all with dispatched status.
            let is_dispatched: bool = conn.query_row(
                "SELECT status = 'dispatched' FROM schedule_fires WHERE fire_id = ?1",
                params![fire_id],
                |row| row.get::<_, bool>(0),
            ).optional()?.unwrap_or(false);

            if !is_dispatched {
                return Ok(false);
            }

            // Fire is dispatched with thread_id — update to clear thread_id
            // so redispatch can proceed. The thread row check is done by the caller.
            conn.execute(
                "UPDATE schedule_fires SET fired_at = ?1 WHERE fire_id = ?2",
                params![lillux::time::timestamp_millis(), fire_id],
            )?;
            return Ok(true);
        }

        // No thread_id at all — safe to reclaim
        conn.execute(
            "UPDATE schedule_fires SET fired_at = ?1 WHERE fire_id = ?2",
            params![lillux::time::timestamp_millis(), fire_id],
        )?;
        Ok(true)
    }

    pub fn get_fire(&self, fire_id: &str) -> Result<Option<FireRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                    status, trigger_reason, outcome, signer_fingerprint
             FROM schedule_fires WHERE fire_id = ?1",
        )?;
        stmt.query_row(params![fire_id], |row| Ok(row_to_fire(row)))
            .optional()
            .map_err(Into::into)
    }

    /// Get the set of existing fire_ids for a schedule. Used by misfire
    /// detection to batch-check which candidate fires already exist,
    /// avoiding N+1 individual get_fire() calls.
    pub fn get_existing_fire_ids(&self, schedule_id: &str) -> Result<std::collections::HashSet<String>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT fire_id FROM schedule_fires WHERE schedule_id = ?1",
        )?;
        let rows = stmt.query_map(params![schedule_id], |row| row.get::<_, String>(0))?;
        let mut ids = std::collections::HashSet::new();
        for row in rows {
            let id = row?;
            ids.insert(id);
        }
        Ok(ids)
    }

    /// Batch-get last fire for multiple schedules. Returns a map from
    /// schedule_id → FireRecord. Used by scheduler_list to avoid N+1 queries.
    pub fn get_last_fires_batch(&self, schedule_ids: &[String]) -> Result<std::collections::HashMap<String, FireRecord>> {
        if schedule_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let placeholders: Vec<String> = schedule_ids.iter().enumerate().map(|(i, _)| format!("?{}", i + 1)).collect();
        let sql = format!(
            "SELECT fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                    status, trigger_reason, outcome, signer_fingerprint
             FROM schedule_fires
             WHERE (schedule_id, scheduled_at) IN (
                 SELECT schedule_id, MAX(scheduled_at)
                 FROM schedule_fires
                 WHERE schedule_id IN ({})
                 GROUP BY schedule_id
             )",
            placeholders.join(","),
        );
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = schedule_ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(params.as_slice(), |row| Ok(row_to_fire(row)))?;
        let mut map = std::collections::HashMap::new();
        for row in rows {
            let rec = row?;
            map.insert(rec.schedule_id.clone(), rec);
        }
        Ok(map)
    }

    pub fn get_last_fire(&self, schedule_id: &str) -> Result<Option<FireRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                    status, trigger_reason, outcome, signer_fingerprint
             FROM schedule_fires
             WHERE schedule_id = ?1
             ORDER BY scheduled_at DESC LIMIT 1",
        )?;
        stmt.query_row(params![schedule_id], |row| Ok(row_to_fire(row)))
            .optional()
            .map_err(Into::into)
    }

    pub fn get_inflight_fires(&self) -> Result<Vec<FireRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                    status, trigger_reason, outcome, signer_fingerprint
             FROM schedule_fires WHERE status = 'dispatched'",
        )?;
        let rows = stmt.query_map([], |row| Ok(row_to_fire(row)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn get_inflight_for_schedule(&self, schedule_id: &str) -> Result<Option<FireRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                    status, trigger_reason, outcome, signer_fingerprint
             FROM schedule_fires
             WHERE schedule_id = ?1 AND status = 'dispatched'
             ORDER BY scheduled_at DESC LIMIT 1",
        )?;
        stmt.query_row(params![schedule_id], |row| Ok(row_to_fire(row)))
            .optional()
            .map_err(Into::into)
    }

    pub fn find_fire_by_thread(&self, thread_id: &str) -> Result<Option<FireRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                    status, trigger_reason, outcome, signer_fingerprint
             FROM schedule_fires
             WHERE thread_id = ?1 AND status = 'dispatched'",
        )?;
        stmt.query_row(params![thread_id], |row| Ok(row_to_fire(row)))
            .optional()
            .map_err(Into::into)
    }

    /// Find dispatched fires older than `threshold_secs` that may need repair.
    /// Used by the periodic repair sweep to finalize stale fires.
    pub fn find_stale_dispatched_fires(&self, threshold_secs: i64) -> Result<Vec<FireRecord>> {
        let cutoff = lillux::time::timestamp_millis() - (threshold_secs * 1000);
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                    status, trigger_reason, outcome, signer_fingerprint
             FROM schedule_fires
             WHERE status = 'dispatched' AND fired_at IS NOT NULL AND fired_at < ?1",
        )?;
        let rows = stmt.query_map(params![cutoff], |row| Ok(row_to_fire(row)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Count fires for a schedule (optionally filtered by status).
    pub fn count_fires(&self, schedule_id: &str, status_filter: Option<&str>) -> Result<usize> {
        let conn = self.lock()?;
        let total: usize = match status_filter {
            Some(s) => conn.query_row(
                "SELECT COUNT(*) FROM schedule_fires WHERE schedule_id = ?1 AND status = ?2",
                params![schedule_id, s],
                |row| row.get::<_, i64>(0),
            )? as usize,
            None => conn.query_row(
                "SELECT COUNT(*) FROM schedule_fires WHERE schedule_id = ?1",
                params![schedule_id],
                |row| row.get::<_, i64>(0),
            )? as usize,
        };
        Ok(total)
    }

    pub fn delete_fires_for_schedule(&self, schedule_id: &str) -> Result<usize> {
        let n = self.lock()?.execute(
            "DELETE FROM schedule_fires WHERE schedule_id = ?1",
            params![schedule_id],
        )?;
        Ok(n)
    }

    pub fn list_fires(
        &self,
        schedule_id: &str,
        status_filter: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<FireRecord>, usize)> {
        let conn = self.lock()?;

        let total: usize = match status_filter {
            Some(s) => conn.query_row(
                "SELECT COUNT(*) FROM schedule_fires WHERE schedule_id = ?1 AND status = ?2",
                params![schedule_id, s],
                |row| row.get::<_, i64>(0),
            )? as usize,
            None => conn.query_row(
                "SELECT COUNT(*) FROM schedule_fires WHERE schedule_id = ?1",
                params![schedule_id],
                |row| row.get::<_, i64>(0),
            )? as usize,
        };

        let sql = match status_filter {
            Some(_) => "SELECT fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                               status, trigger_reason, outcome, signer_fingerprint
                        FROM schedule_fires
                        WHERE schedule_id = ?1 AND status = ?2
                        ORDER BY scheduled_at DESC LIMIT ?3",
            None => "SELECT fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                            status, trigger_reason, outcome, signer_fingerprint
                     FROM schedule_fires
                     WHERE schedule_id = ?1
                     ORDER BY scheduled_at DESC LIMIT ?2",
        };
        let mut stmt = conn.prepare(sql)?;
        let fires: Vec<FireRecord> = if let Some(sf) = status_filter {
            stmt.query_map(params![schedule_id, sf, limit as i64], |row| Ok(row_to_fire(row)))?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![schedule_id, limit as i64], |row| Ok(row_to_fire(row)))?
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok((fires, total))
    }
}

// ── Row mappers ─────────────────────────────────────────────────────

fn row_to_spec(row: &rusqlite::Row<'_>) -> ScheduleSpecRecord {
    ScheduleSpecRecord {
        schedule_id: row.get("schedule_id").unwrap(),
        item_ref: row.get("item_ref").unwrap(),
        params: row.get("params").unwrap(),
        schedule_type: row.get("schedule_type").unwrap(),
        expression: row.get("expression").unwrap(),
        timezone: row.get("timezone").unwrap(),
        misfire_policy: row.get("misfire_policy").unwrap(),
        overlap_policy: row.get("overlap_policy").unwrap(),
        enabled: row.get::<_, i32>("enabled").unwrap() != 0,
        project_root: row.get("project_root").unwrap(),
        signer_fingerprint: row.get("signer_fingerprint").unwrap(),
        spec_hash: row.get("spec_hash").unwrap(),
        last_modified: row.get("last_modified").unwrap(),
    }
}

fn row_to_fire(row: &rusqlite::Row<'_>) -> FireRecord {
    FireRecord {
        fire_id: row.get("fire_id").unwrap(),
        schedule_id: row.get("schedule_id").unwrap(),
        scheduled_at: row.get("scheduled_at").unwrap(),
        fired_at: row.get("fired_at").unwrap(),
        completed_at: row.get("completed_at").unwrap(),
        thread_id: row.get("thread_id").unwrap(),
        status: row.get("status").unwrap(),
        trigger_reason: row.get("trigger_reason").unwrap(),
        outcome: row.get("outcome").unwrap(),
        signer_fingerprint: row.get("signer_fingerprint").unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_db() -> SchedulerDb {
        SchedulerDb::open(&PathBuf::from(":memory:")).expect("open in-memory scheduler db")
    }

    fn make_spec(id: &str) -> ScheduleSpecRecord {
        ScheduleSpecRecord {
            schedule_id: id.to_string(),
            item_ref: "directive:test".to_string(),
            params: r#"{"key":"value"}"#.to_string(),
            schedule_type: "interval".to_string(),
            expression: "60".to_string(),
            timezone: "UTC".to_string(),
            misfire_policy: "skip".to_string(),
            overlap_policy: "skip".to_string(),
            enabled: true,
            project_root: None,
            signer_fingerprint: "fp:test".to_string(),
            spec_hash: "abc123".to_string(),
            last_modified: 1000,
        }
    }

    fn make_fire(schedule_id: &str, scheduled_at: i64, status: &str) -> FireRecord {
        FireRecord {
            fire_id: format!("{}@{}", schedule_id, scheduled_at),
            schedule_id: schedule_id.to_string(),
            scheduled_at,
            fired_at: Some(1001),
            completed_at: None,
            thread_id: Some(format!("sched-{:032x}", scheduled_at)),
            status: status.to_string(),
            trigger_reason: "normal".to_string(),
            outcome: None,
            signer_fingerprint: Some("fp:test".to_string()),
        }
    }

    // ── Spec CRUD ──────────────────────────────────────────────

    #[test]
    fn upsert_and_get_spec() {
        let db = test_db();
        let spec = make_spec("test-sched");
        db.upsert_spec(&spec).unwrap();

        let got = db.get_spec("test-sched").unwrap().unwrap();
        assert_eq!(got.schedule_id, "test-sched");
        assert_eq!(got.item_ref, "directive:test");
        assert_eq!(got.expression, "60");
        assert!(got.enabled);
    }

    #[test]
    fn get_spec_nonexistent() {
        let db = test_db();
        assert!(db.get_spec("nope").unwrap().is_none());
    }

    #[test]
    fn upsert_spec_updates_existing() {
        let db = test_db();
        let mut spec = make_spec("sched");
        spec.expression = "30".to_string();
        db.upsert_spec(&spec).unwrap();

        let mut spec2 = make_spec("sched");
        spec2.expression = "120".to_string();
        db.upsert_spec(&spec2).unwrap();

        let got = db.get_spec("sched").unwrap().unwrap();
        assert_eq!(got.expression, "120");
    }

    #[test]
    fn delete_spec() {
        let db = test_db();
        db.upsert_spec(&make_spec("sched")).unwrap();
        assert!(db.delete_spec("sched").unwrap());
        assert!(db.get_spec("sched").unwrap().is_none());
    }

    #[test]
    fn delete_spec_nonexistent() {
        let db = test_db();
        assert!(!db.delete_spec("nope").unwrap());
    }

    #[test]
    fn load_enabled_specs() {
        let db = test_db();
        db.upsert_spec(&make_spec("a")).unwrap();
        db.upsert_spec(&make_spec("b")).unwrap();

        let mut spec_c = make_spec("c");
        spec_c.enabled = false;
        db.upsert_spec(&spec_c).unwrap();

        let specs = db.load_enabled_specs().unwrap();
        assert_eq!(specs.len(), 2);
        let ids: Vec<&str> = specs.iter().map(|s| s.schedule_id.as_str()).collect();
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"b"));
        assert!(!ids.contains(&"c"));
    }

    #[test]
    fn list_specs_with_type_filter() {
        let db = test_db();
        let mut cron_spec = make_spec("cron-sched");
        cron_spec.schedule_type = "cron".to_string();
        cron_spec.expression = "* * * * * *".to_string();
        db.upsert_spec(&cron_spec).unwrap();
        db.upsert_spec(&make_spec("interval-sched")).unwrap();

        let specs = db.list_specs(false, Some("cron")).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].schedule_type, "cron");
    }

    #[test]
    fn list_specs_enabled_only() {
        let db = test_db();
        db.upsert_spec(&make_spec("enabled")).unwrap();
        let mut disabled = make_spec("disabled");
        disabled.enabled = false;
        db.upsert_spec(&disabled).unwrap();

        let specs = db.list_specs(true, None).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].schedule_id, "enabled");
    }

    #[test]
    fn delete_stale_specs() {
        let db = test_db();
        db.upsert_spec(&make_spec("keep-me")).unwrap();
        db.upsert_spec(&make_spec("remove-me")).unwrap();

        let removed = db.delete_stale_specs(&["keep-me"]).unwrap();
        assert_eq!(removed, 1);
        assert!(db.get_spec("keep-me").unwrap().is_some());
        assert!(db.get_spec("remove-me").unwrap().is_none());
    }

    #[test]
    fn delete_stale_specs_empty_live() {
        let db = test_db();
        db.upsert_spec(&make_spec("a")).unwrap();
        db.upsert_spec(&make_spec("b")).unwrap();

        let removed = db.delete_stale_specs(&[]).unwrap();
        assert_eq!(removed, 2);
        assert_eq!(db.load_enabled_specs().unwrap().len(), 0);
    }

    // ── Fire CRUD ──────────────────────────────────────────────

    #[test]
    fn upsert_and_get_fire() {
        let db = test_db();
        let fire = make_fire("sched", 1000, "dispatched");
        db.upsert_fire(&fire).unwrap();

        let got = db.get_fire("sched@1000").unwrap().unwrap();
        assert_eq!(got.fire_id, "sched@1000");
        assert_eq!(got.status, "dispatched");
    }

    #[test]
    fn get_fire_nonexistent() {
        let db = test_db();
        assert!(db.get_fire("nope").unwrap().is_none());
    }

    #[test]
    fn upsert_fire_updates_status() {
        let db = test_db();
        let fire = make_fire("sched", 1000, "dispatched");
        db.upsert_fire(&fire).unwrap();

        let updated = FireRecord {
            status: "completed".to_string(),
            outcome: Some("success".to_string()),
            ..fire
        };
        db.upsert_fire(&updated).unwrap();

        let got = db.get_fire("sched@1000").unwrap().unwrap();
        assert_eq!(got.status, "completed");
        assert_eq!(got.outcome.unwrap(), "success");
    }

    #[test]
    fn get_last_fire() {
        let db = test_db();
        db.upsert_fire(&make_fire("sched", 1000, "completed")).unwrap();
        db.upsert_fire(&make_fire("sched", 2000, "dispatched")).unwrap();

        let last = db.get_last_fire("sched").unwrap().unwrap();
        assert_eq!(last.scheduled_at, 2000);
    }

    #[test]
    fn get_last_fire_empty() {
        let db = test_db();
        assert!(db.get_last_fire("sched").unwrap().is_none());
    }

    #[test]
    fn get_inflight_fires() {
        let db = test_db();
        db.upsert_fire(&make_fire("sched", 1000, "completed")).unwrap();
        db.upsert_fire(&make_fire("sched", 2000, "dispatched")).unwrap();
        db.upsert_fire(&make_fire("other", 3000, "dispatched")).unwrap();

        let inflight = db.get_inflight_fires().unwrap();
        assert_eq!(inflight.len(), 2);
    }

    #[test]
    fn get_inflight_for_schedule() {
        let db = test_db();
        db.upsert_fire(&make_fire("sched", 1000, "completed")).unwrap();
        db.upsert_fire(&make_fire("sched", 2000, "dispatched")).unwrap();
        db.upsert_fire(&make_fire("other", 3000, "dispatched")).unwrap();

        let inflight = db.get_inflight_for_schedule("sched").unwrap().unwrap();
        assert_eq!(inflight.scheduled_at, 2000);
    }

    #[test]
    fn get_inflight_for_schedule_none() {
        let db = test_db();
        assert!(db.get_inflight_for_schedule("sched").unwrap().is_none());
    }

    #[test]
    fn find_fire_by_thread() {
        let db = test_db();
        let fire = make_fire("sched", 1000, "dispatched");
        db.upsert_fire(&fire).unwrap();

        let found = db.find_fire_by_thread(fire.thread_id.as_ref().unwrap()).unwrap().unwrap();
        assert_eq!(found.fire_id, "sched@1000");
    }

    #[test]
    fn find_fire_by_thread_completed_not_found() {
        let db = test_db();
        let fire = make_fire("sched", 1000, "completed");
        db.upsert_fire(&fire).unwrap();

        let found = db.find_fire_by_thread(fire.thread_id.as_ref().unwrap()).unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn delete_fires_for_schedule() {
        let db = test_db();
        db.upsert_fire(&make_fire("sched", 1000, "completed")).unwrap();
        db.upsert_fire(&make_fire("sched", 2000, "dispatched")).unwrap();
        db.upsert_fire(&make_fire("other", 3000, "dispatched")).unwrap();

        let removed = db.delete_fires_for_schedule("sched").unwrap();
        assert_eq!(removed, 2);
        assert!(db.get_fire("sched@1000").unwrap().is_none());
        assert!(db.get_fire("other@3000").unwrap().is_some());
    }

    #[test]
    fn list_fires_with_status_filter() {
        let db = test_db();
        db.upsert_fire(&make_fire("sched", 1000, "completed")).unwrap();
        db.upsert_fire(&make_fire("sched", 2000, "dispatched")).unwrap();
        db.upsert_fire(&make_fire("sched", 3000, "completed")).unwrap();

        let (fires, total) = db.list_fires("sched", Some("completed"), 10).unwrap();
        assert_eq!(total, 2);
        assert_eq!(fires.len(), 2);
    }

    #[test]
    fn list_fires_with_limit() {
        let db = test_db();
        db.upsert_fire(&make_fire("sched", 1000, "completed")).unwrap();
        db.upsert_fire(&make_fire("sched", 2000, "completed")).unwrap();
        db.upsert_fire(&make_fire("sched", 3000, "completed")).unwrap();

        let (fires, total) = db.list_fires("sched", None, 2).unwrap();
        assert_eq!(total, 3);
        assert_eq!(fires.len(), 2);
        assert_eq!(fires[0].scheduled_at, 3000);
        assert_eq!(fires[1].scheduled_at, 2000);
    }

    #[test]
    fn list_fires_empty() {
        let db = test_db();
        let (fires, total) = db.list_fires("sched", None, 10).unwrap();
        assert_eq!(total, 0);
        assert!(fires.is_empty());
    }
}
