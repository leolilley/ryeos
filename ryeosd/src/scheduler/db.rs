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
                (fire_id, schedule_id, scheduled_at, fired_at, thread_id,
                 status, trigger_reason, outcome, signer_fingerprint)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(fire_id) DO UPDATE SET
                status=excluded.status, fired_at=COALESCE(excluded.fired_at, fired_at),
                thread_id=COALESCE(excluded.thread_id, thread_id),
                outcome=COALESCE(excluded.outcome, outcome),
                trigger_reason=excluded.trigger_reason,
                signer_fingerprint=COALESCE(excluded.signer_fingerprint, signer_fingerprint)",
            params![
                rec.fire_id, rec.schedule_id, rec.scheduled_at,
                rec.fired_at, rec.thread_id, rec.status,
                rec.trigger_reason, rec.outcome, rec.signer_fingerprint,
            ],
        )
        .with_context(|| format!("upsert_fire failed for {}", rec.fire_id))?;
        Ok(())
    }

    pub fn get_fire(&self, fire_id: &str) -> Result<Option<FireRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT fire_id, schedule_id, scheduled_at, fired_at, thread_id,
                    status, trigger_reason, outcome, signer_fingerprint
             FROM schedule_fires WHERE fire_id = ?1",
        )?;
        stmt.query_row(params![fire_id], |row| Ok(row_to_fire(row)))
            .optional()
            .map_err(Into::into)
    }

    pub fn get_last_fire(&self, schedule_id: &str) -> Result<Option<FireRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT fire_id, schedule_id, scheduled_at, fired_at, thread_id,
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
            "SELECT fire_id, schedule_id, scheduled_at, fired_at, thread_id,
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
            "SELECT fire_id, schedule_id, scheduled_at, fired_at, thread_id,
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
            "SELECT fire_id, schedule_id, scheduled_at, fired_at, thread_id,
                    status, trigger_reason, outcome, signer_fingerprint
             FROM schedule_fires
             WHERE thread_id = ?1 AND status = 'dispatched'",
        )?;
        stmt.query_row(params![thread_id], |row| Ok(row_to_fire(row)))
            .optional()
            .map_err(Into::into)
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
            Some(_) => "SELECT fire_id, schedule_id, scheduled_at, fired_at, thread_id,
                               status, trigger_reason, outcome, signer_fingerprint
                        FROM schedule_fires
                        WHERE schedule_id = ?1 AND status = ?2
                        ORDER BY scheduled_at DESC LIMIT ?3",
            None => "SELECT fire_id, schedule_id, scheduled_at, fired_at, thread_id,
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
        thread_id: row.get("thread_id").unwrap(),
        status: row.get("status").unwrap(),
        trigger_reason: row.get("trigger_reason").unwrap(),
        outcome: row.get("outcome").unwrap(),
        signer_fingerprint: row.get("signer_fingerprint").unwrap(),
    }
}
