use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use serde_json::{json, Value};

use crate::kind_profiles::KindProfileRegistry;

const MIGRATIONS: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;

CREATE TABLE IF NOT EXISTS threads (
    thread_id TEXT PRIMARY KEY,
    chain_root_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN (
        'created',
        'running',
        'completed',
        'failed',
        'cancelled',
        'killed',
        'timed_out',
        'continued'
    )),
    item_ref TEXT NOT NULL,
    executor_ref TEXT NOT NULL,
    launch_mode TEXT NOT NULL CHECK (launch_mode IN ('inline', 'detached')),
    current_site_id TEXT NOT NULL,
    origin_site_id TEXT NOT NULL,
    upstream_thread_id TEXT,
    requested_by TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    summary_json BLOB
);

CREATE INDEX IF NOT EXISTS idx_threads_chain_root ON threads(chain_root_id);
CREATE INDEX IF NOT EXISTS idx_threads_status ON threads(status);
CREATE INDEX IF NOT EXISTS idx_threads_kind ON threads(kind);
CREATE INDEX IF NOT EXISTS idx_threads_current_site ON threads(current_site_id);
CREATE INDEX IF NOT EXISTS idx_threads_origin_site ON threads(origin_site_id);

CREATE TABLE IF NOT EXISTS thread_edges (
    edge_id INTEGER PRIMARY KEY AUTOINCREMENT,
    chain_root_id TEXT NOT NULL,
    source_thread_id TEXT NOT NULL REFERENCES threads(thread_id),
    target_thread_id TEXT NOT NULL REFERENCES threads(thread_id),
    edge_type TEXT NOT NULL CHECK (edge_type IN (
        'spawned',
        'continued',
        'waits_on',
        'triggered_by',
        'mirrored_from'
    )),
    created_at TEXT NOT NULL,
    metadata BLOB,
    UNIQUE(source_thread_id, target_thread_id, edge_type)
);

CREATE INDEX IF NOT EXISTS idx_thread_edges_chain_root ON thread_edges(chain_root_id);
CREATE INDEX IF NOT EXISTS idx_thread_edges_source ON thread_edges(source_thread_id);
CREATE INDEX IF NOT EXISTS idx_thread_edges_target ON thread_edges(target_thread_id);

CREATE TABLE IF NOT EXISTS thread_runtime (
    thread_id TEXT PRIMARY KEY REFERENCES threads(thread_id),
    pid INTEGER,
    pgid INTEGER,
    provider TEXT,
    turns INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    spend REAL NOT NULL DEFAULT 0.0,
    metadata BLOB
);

CREATE TABLE IF NOT EXISTS thread_results (
    thread_id TEXT PRIMARY KEY REFERENCES threads(thread_id),
    outcome_code TEXT,
    result_json BLOB,
    error_json BLOB,
    metadata BLOB
);

CREATE TABLE IF NOT EXISTS thread_artifacts (
    artifact_id INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id TEXT NOT NULL REFERENCES threads(thread_id),
    artifact_type TEXT NOT NULL,
    uri TEXT NOT NULL,
    content_hash TEXT,
    metadata BLOB
);

CREATE INDEX IF NOT EXISTS idx_thread_artifacts_thread ON thread_artifacts(thread_id);
CREATE INDEX IF NOT EXISTS idx_thread_artifacts_type ON thread_artifacts(artifact_type);

CREATE TABLE IF NOT EXISTS thread_budgets (
    thread_id TEXT PRIMARY KEY REFERENCES threads(thread_id),
    budget_parent_id TEXT REFERENCES threads(thread_id),
    reserved_spend REAL NOT NULL DEFAULT 0.0,
    actual_spend REAL NOT NULL DEFAULT 0.0,
    status TEXT NOT NULL,
    metadata BLOB
);

CREATE INDEX IF NOT EXISTS idx_thread_budgets_parent ON thread_budgets(budget_parent_id);

CREATE TABLE IF NOT EXISTS thread_commands (
    command_id INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id TEXT NOT NULL REFERENCES threads(thread_id),
    command_type TEXT NOT NULL,
    status TEXT NOT NULL,
    requested_by TEXT,
    params BLOB,
    result BLOB,
    created_at TEXT NOT NULL,
    claimed_at TEXT,
    completed_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_thread_commands_thread_status ON thread_commands(thread_id, status);

CREATE TABLE IF NOT EXISTS events (
    event_id INTEGER PRIMARY KEY AUTOINCREMENT,
    chain_root_id TEXT NOT NULL,
    chain_seq INTEGER NOT NULL,
    thread_id TEXT NOT NULL REFERENCES threads(thread_id),
    thread_seq INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    storage_class TEXT NOT NULL,
    ts TEXT NOT NULL,
    payload BLOB NOT NULL,
    UNIQUE(chain_root_id, chain_seq),
    UNIQUE(thread_id, thread_seq)
);

CREATE INDEX IF NOT EXISTS idx_events_thread ON events(thread_id);
CREATE INDEX IF NOT EXISTS idx_events_chain ON events(chain_root_id, chain_seq);
CREATE INDEX IF NOT EXISTS idx_events_type ON events(event_type);

CREATE TABLE IF NOT EXISTS event_replay_index (
    replay_id INTEGER PRIMARY KEY AUTOINCREMENT,
    chain_root_id TEXT NOT NULL,
    chain_seq INTEGER NOT NULL,
    thread_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    ts TEXT NOT NULL,
    payload BLOB NOT NULL,
    UNIQUE(chain_root_id, chain_seq)
);

CREATE INDEX IF NOT EXISTS idx_event_replay_chain ON event_replay_index(chain_root_id, chain_seq);
CREATE INDEX IF NOT EXISTS idx_event_replay_thread ON event_replay_index(thread_id, chain_seq);

CREATE TABLE IF NOT EXISTS chain_counters (
    chain_root_id TEXT PRIMARY KEY,
    next_chain_seq INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS thread_counters (
    thread_id TEXT PRIMARY KEY,
    next_thread_seq INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS thread_facets (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id TEXT NOT NULL REFERENCES threads(thread_id),
    facet_key TEXT NOT NULL,
    facet_value TEXT NOT NULL,
    UNIQUE(thread_id, facet_key)
);

CREATE INDEX IF NOT EXISTS idx_thread_facets_thread ON thread_facets(thread_id);
CREATE INDEX IF NOT EXISTS idx_thread_facets_key_value ON thread_facets(facet_key, facet_value);
"#;

#[derive(Debug, Clone)]
pub struct Database {
    path: PathBuf,
    kind_profiles: Arc<KindProfileRegistry>,
}

#[derive(Debug, Serialize)]
pub struct ThreadListItem {
    pub thread_id: String,
    pub chain_root_id: String,
    pub kind: String,
    pub status: String,
    pub item_ref: String,
    pub launch_mode: String,
    pub current_site_id: String,
    pub origin_site_id: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize)]
pub struct RuntimeInfo {
    pub pid: Option<i64>,
    pub pgid: Option<i64>,
    pub provider: Option<String>,
    pub turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub spend: f64,
}

#[derive(Debug, Serialize)]
pub struct BudgetInfo {
    pub budget_parent_id: Option<String>,
    pub reserved_spend: f64,
    pub actual_spend: f64,
    pub status: String,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct NewThreadRecord {
    pub thread_id: String,
    pub chain_root_id: String,
    pub kind: String,
    pub status: String,
    pub item_ref: String,
    pub executor_ref: String,
    pub launch_mode: String,
    pub current_site_id: String,
    pub origin_site_id: String,
    pub upstream_thread_id: Option<String>,
    pub requested_by: Option<String>,
    pub summary_json: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct ThreadBudgetRecord {
    pub budget_parent_id: Option<String>,
    pub reserved_spend: f64,
    pub actual_spend: f64,
    pub status: String,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadArtifactRecord {
    pub artifact_id: i64,
    pub artifact_type: String,
    pub uri: String,
    pub content_hash: Option<String>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadEdgeRecord {
    pub edge_id: i64,
    pub chain_root_id: String,
    pub source_thread_id: String,
    pub target_thread_id: String,
    pub edge_type: String,
    pub created_at: String,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct NewArtifactRecord {
    pub artifact_type: String,
    pub uri: String,
    pub content_hash: Option<String>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct NewEventRecord {
    pub event_type: String,
    pub storage_class: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct PersistedEventRecord {
    pub event_id: i64,
    pub chain_root_id: String,
    pub chain_seq: i64,
    pub thread_id: String,
    pub thread_seq: i64,
    pub event_type: String,
    pub storage_class: String,
    pub ts: String,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct NewCommandRecord {
    pub thread_id: String,
    pub command_type: String,
    pub requested_by: Option<String>,
    pub params: Option<Value>,
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
pub struct RuntimeCostRecord {
    pub provider: Option<String>,
    pub turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub spend: f64,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct FinalizeThreadRecord {
    pub status: String,
    pub outcome_code: Option<String>,
    pub result_json: Option<Value>,
    pub error_json: Option<Value>,
    pub metadata: Option<Value>,
    pub artifacts: Vec<NewArtifactRecord>,
    pub final_cost: Option<RuntimeCostRecord>,
    pub summary_json: Option<Value>,
    pub budget_status: Option<String>,
    pub budget_metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadResultRecord {
    pub outcome_code: Option<String>,
    pub result: Option<Value>,
    pub error: Option<Value>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct ThreadDetail {
    pub thread_id: String,
    pub chain_root_id: String,
    pub kind: String,
    pub status: String,
    pub item_ref: String,
    pub executor_ref: String,
    pub launch_mode: String,
    pub current_site_id: String,
    pub origin_site_id: String,
    pub upstream_thread_id: Option<String>,
    pub requested_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub runtime: RuntimeInfo,
    pub budget: Option<BudgetInfo>,
    pub allowed_actions: Vec<String>,
}

impl Database {
    pub fn new(path: impl AsRef<Path>, kind_profiles: Arc<KindProfileRegistry>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create db dir {}", parent.display()))?;
        }

        let db = Self {
            path,
            kind_profiles,
        };
        db.migrate()?;
        Ok(db)
    }

    pub fn kind_profiles(&self) -> &KindProfileRegistry {
        &self.kind_profiles
    }

    pub fn list_threads(&self, limit: usize) -> Result<Vec<ThreadListItem>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT thread_id, chain_root_id, kind, status, item_ref, launch_mode,
                    current_site_id, origin_site_id, created_at, updated_at
             FROM threads
             ORDER BY created_at DESC
             LIMIT ?1",
        )?;

        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(ThreadListItem {
                thread_id: row.get(0)?,
                chain_root_id: row.get(1)?,
                kind: row.get(2)?,
                status: row.get(3)?,
                item_ref: row.get(4)?,
                launch_mode: row.get(5)?,
                current_site_id: row.get(6)?,
                origin_site_id: row.get(7)?,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?;

        let mut items = Vec::new();
        for row in rows {
            items.push(row?);
        }
        Ok(items)
    }

    pub fn list_threads_by_status(&self, statuses: &[&str]) -> Result<Vec<ThreadDetail>> {
        if statuses.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders: Vec<String> = statuses.iter().enumerate().map(|(i, _)| format!("?{}", i + 1)).collect();
        let sql = format!(
            "SELECT thread_id FROM threads WHERE status IN ({}) ORDER BY created_at DESC",
            placeholders.join(", ")
        );
        let conn = self.connect()?;
        let mut stmt = conn.prepare(&sql)?;
        let ids: Vec<String> = stmt
            .query_map(
                rusqlite::params_from_iter(statuses.iter()),
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        drop(conn);

        let mut results = Vec::new();
        for id in &ids {
            if let Some(detail) = self.get_thread(id)? {
                results.push(detail);
            }
        }
        Ok(results)
    }

    pub fn get_thread(&self, thread_id: &str) -> Result<Option<ThreadDetail>> {
        let conn = self.connect()?;

        let base = conn
            .query_row(
                "SELECT thread_id, chain_root_id, kind, status, item_ref, executor_ref,
                        launch_mode, current_site_id, origin_site_id, upstream_thread_id,
                        requested_by, created_at, updated_at, started_at, finished_at
                 FROM threads WHERE thread_id = ?1",
                params![thread_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, Option<String>>(10)?,
                        row.get::<_, String>(11)?,
                        row.get::<_, String>(12)?,
                        row.get::<_, Option<String>>(13)?,
                        row.get::<_, Option<String>>(14)?,
                    ))
                },
            )
            .optional()?;

        let Some(base) = base else {
            return Ok(None);
        };

        let runtime = conn
            .query_row(
                "SELECT pid, pgid, provider, turns, input_tokens, output_tokens, spend
                 FROM thread_runtime WHERE thread_id = ?1",
                params![thread_id],
                |row| {
                    Ok(RuntimeInfo {
                        pid: row.get(0)?,
                        pgid: row.get(1)?,
                        provider: row.get(2)?,
                        turns: row.get(3)?,
                        input_tokens: row.get(4)?,
                        output_tokens: row.get(5)?,
                        spend: row.get(6)?,
                    })
                },
            )
            .optional()?
            .unwrap_or(RuntimeInfo {
                pid: None,
                pgid: None,
                provider: None,
                turns: 0,
                input_tokens: 0,
                output_tokens: 0,
                spend: 0.0,
            });

        let budget = conn
            .query_row(
                "SELECT budget_parent_id, reserved_spend, actual_spend, status
                        , metadata
                 FROM thread_budgets WHERE thread_id = ?1",
                params![thread_id],
                |row| {
                    Ok(BudgetInfo {
                        budget_parent_id: row.get(0)?,
                        reserved_spend: row.get(1)?,
                        actual_spend: row.get(2)?,
                        status: row.get(3)?,
                        metadata: parse_json_blob(row.get(4)?)?,
                    })
                },
            )
            .optional()?;

        let profile = self.kind_profiles.get(&base.2);
        let allowed_actions = derive_allowed_actions(profile, &base.3, runtime.pgid.is_some());

        Ok(Some(ThreadDetail {
            thread_id: base.0,
            chain_root_id: base.1,
            kind: base.2,
            status: base.3,
            item_ref: base.4,
            executor_ref: base.5,
            launch_mode: base.6,
            current_site_id: base.7,
            origin_site_id: base.8,
            upstream_thread_id: base.9,
            requested_by: base.10,
            created_at: base.11,
            updated_at: base.12,
            started_at: base.13,
            finished_at: base.14,
            runtime,
            budget,
            allowed_actions,
        }))
    }

    pub fn create_thread(
        &self,
        thread: &NewThreadRecord,
        budget: Option<&ThreadBudgetRecord>,
    ) -> Result<Vec<PersistedEventRecord>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_rfc3339();

        tx.execute(
            "INSERT INTO threads (
                thread_id, chain_root_id, kind, status, item_ref, executor_ref,
                launch_mode, current_site_id, origin_site_id, upstream_thread_id,
                requested_by, created_at, updated_at, summary_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                &thread.thread_id,
                &thread.chain_root_id,
                &thread.kind,
                &thread.status,
                &thread.item_ref,
                &thread.executor_ref,
                &thread.launch_mode,
                &thread.current_site_id,
                &thread.origin_site_id,
                &thread.upstream_thread_id,
                &thread.requested_by,
                now,
                now,
                json_blob(&thread.summary_json)?,
            ],
        )?;

        tx.execute(
            "INSERT INTO thread_runtime (
                thread_id, pid, pgid, provider, turns, input_tokens, output_tokens, spend, metadata
             ) VALUES (?1, NULL, NULL, NULL, 0, 0, 0, 0.0, NULL)",
            params![&thread.thread_id],
        )?;

        if let Some(upstream_thread_id) = &thread.upstream_thread_id {
            let parent = tx
                .query_row(
                    "SELECT chain_root_id FROM threads WHERE thread_id = ?1",
                    params![upstream_thread_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .ok_or_else(|| anyhow!("upstream thread not found: {upstream_thread_id}"))?;
            if parent != thread.chain_root_id {
                bail!(
                    "child thread chain_root_id {} does not match upstream chain_root_id {}",
                    thread.chain_root_id,
                    parent
                );
            }

            tx.execute(
                "INSERT INTO thread_edges (
                    chain_root_id, source_thread_id, target_thread_id, edge_type, created_at, metadata
                 ) VALUES (?1, ?2, ?3, 'spawned', ?4, NULL)",
                params![
                    &thread.chain_root_id,
                    upstream_thread_id,
                    &thread.thread_id,
                    now,
                ],
            )?;
        }

        if let Some(budget) = budget {
            tx.execute(
                "INSERT INTO thread_budgets (
                    thread_id, budget_parent_id, reserved_spend, actual_spend, status, metadata
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    &thread.thread_id,
                    &budget.budget_parent_id,
                    budget.reserved_spend,
                    budget.actual_spend,
                    &budget.status,
                    json_blob(&budget.metadata)?,
                ],
            )?;
        }

        let mut persisted = append_events_tx(
            &tx,
            &thread.chain_root_id,
            &thread.thread_id,
            &[NewEventRecord {
                event_type: "thread_created".to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({
                    "kind": thread.kind,
                    "item_ref": thread.item_ref,
                    "executor_ref": thread.executor_ref,
                    "launch_mode": thread.launch_mode,
                }),
            }],
        )?;

        if let Some(budget) = budget.filter(|budget| budget.budget_parent_id.is_some()) {
            persisted.extend(append_events_tx(
                &tx,
                &thread.chain_root_id,
                &thread.thread_id,
                &[NewEventRecord {
                    event_type: "budget_reserved".to_string(),
                    storage_class: "indexed".to_string(),
                    payload: json!({
                        "budget_parent_id": budget.budget_parent_id,
                        "reserved_spend": budget.reserved_spend,
                    }),
                }],
            )?);
        }

        if let Some(upstream_thread_id) = &thread.upstream_thread_id {
            persisted.extend(append_events_tx(
                &tx,
                &thread.chain_root_id,
                &thread.thread_id,
                &[
                    NewEventRecord {
                        event_type: "edge_recorded".to_string(),
                        storage_class: "indexed".to_string(),
                        payload: json!({
                            "edge_type": "spawned",
                            "source_thread_id": upstream_thread_id,
                            "target_thread_id": thread.thread_id,
                        }),
                    },
                    NewEventRecord {
                        event_type: "child_thread_spawned".to_string(),
                        storage_class: "indexed".to_string(),
                        payload: json!({
                            "source_thread_id": upstream_thread_id,
                            "target_thread_id": thread.thread_id,
                            "kind": thread.kind,
                        }),
                    },
                ],
            )?);
        }

        tx.commit()?;
        Ok(persisted)
    }

    pub fn create_continuation(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        reason: Option<&str>,
    ) -> Result<Vec<PersistedEventRecord>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_rfc3339();

        // 1. Finalize source thread as 'continued'
        let (_, source_status) = require_thread_identity(&tx, source_thread_id)?;
        if is_terminal_status(&source_status)
            && source_status != "failed"
            && source_status != "completed"
        {
            bail!(
                "cannot continue thread in terminal status '{source_status}'"
            );
        }

        tx.execute(
            "UPDATE threads SET status = 'continued', updated_at = ?2, finished_at = ?2
             WHERE thread_id = ?1",
            params![source_thread_id, now],
        )?;

        // 2. Create successor thread
        tx.execute(
            "INSERT INTO threads (
                thread_id, chain_root_id, kind, status, item_ref, executor_ref,
                launch_mode, current_site_id, origin_site_id, upstream_thread_id,
                requested_by, created_at, updated_at, summary_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12, ?13)",
            params![
                &successor.thread_id,
                chain_root_id,
                &successor.kind,
                "created",
                &successor.item_ref,
                &successor.executor_ref,
                &successor.launch_mode,
                &successor.current_site_id,
                &successor.origin_site_id,
                &successor.upstream_thread_id,
                &successor.requested_by,
                now,
                json_blob(&successor.summary_json)?,
            ],
        )?;

        tx.execute(
            "INSERT INTO thread_runtime (thread_id) VALUES (?1)",
            params![&successor.thread_id],
        )?;

        // 3. Write 'continued' edge
        let edge_metadata = reason.map(|r| json!({ "reason": r }));
        tx.execute(
            "INSERT INTO thread_edges (
                chain_root_id, source_thread_id, target_thread_id, edge_type, created_at, metadata
             ) VALUES (?1, ?2, ?3, 'continued', ?4, ?5)",
            params![
                chain_root_id,
                source_thread_id,
                &successor.thread_id,
                now,
                json_blob(&edge_metadata)?,
            ],
        )?;

        // 4. Emit events
        let mut events = vec![
            NewEventRecord {
                event_type: "thread_continued".to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({
                    "successor_thread_id": &successor.thread_id,
                    "reason": reason,
                }),
            },
        ];
        let source_events = append_events_tx(
            &tx,
            chain_root_id,
            source_thread_id,
            &events,
        )?;

        events.clear();
        events.push(NewEventRecord {
            event_type: "thread_created".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "kind": &successor.kind,
                "item_ref": &successor.item_ref,
                "continuation_from": source_thread_id,
            }),
        });
        let successor_events = append_events_tx(
            &tx,
            chain_root_id,
            &successor.thread_id,
            &events,
        )?;

        tx.commit()?;

        let mut all_events = source_events;
        all_events.extend(successor_events);
        Ok(all_events)
    }

    pub fn mark_thread_running(&self, thread_id: &str) -> Result<Vec<PersistedEventRecord>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (chain_root_id, status) = require_thread_identity(&tx, thread_id)?;
        if status != "created" {
            bail!("invalid status transition: {status} -> running");
        }

        let now = now_rfc3339();
        tx.execute(
            "UPDATE threads
             SET status = 'running', updated_at = ?2, started_at = COALESCE(started_at, ?2)
             WHERE thread_id = ?1",
            params![thread_id, now],
        )?;

        let persisted = append_events_tx(
            &tx,
            &chain_root_id,
            thread_id,
            &[NewEventRecord {
                event_type: "thread_started".to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({}),
            }],
        )?;

        tx.commit()?;
        Ok(persisted)
    }

    pub fn attach_thread_process(
        &self,
        thread_id: &str,
        pid: i64,
        pgid: i64,
        metadata: Option<&Value>,
    ) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE thread_runtime
             SET pid = ?2, pgid = ?3, metadata = COALESCE(?4, metadata)
             WHERE thread_id = ?1",
            params![thread_id, pid, pgid, json_blob_ref(metadata)?],
        )?;
        Ok(())
    }

    pub fn finalize_thread(
        &self,
        thread_id: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<Vec<PersistedEventRecord>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (chain_root_id, status) = require_thread_identity(&tx, thread_id)?;
        if is_terminal_status(&status) {
            bail!("invalid status transition: {status} -> {}", update.status);
        }

        let now = now_rfc3339();

        tx.execute(
            "UPDATE threads
             SET status = ?2,
                 updated_at = ?3,
                 finished_at = ?3,
                 summary_json = COALESCE(?4, summary_json)
             WHERE thread_id = ?1",
            params![
                thread_id,
                &update.status,
                now,
                json_blob(&update.summary_json)?
            ],
        )?;

        tx.execute(
            "INSERT INTO thread_results (thread_id, outcome_code, result_json, error_json, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(thread_id) DO UPDATE SET
                 outcome_code = excluded.outcome_code,
                 result_json = excluded.result_json,
                 error_json = excluded.error_json,
                 metadata = excluded.metadata",
            params![
                thread_id,
                &update.outcome_code,
                json_blob(&update.result_json)?,
                json_blob(&update.error_json)?,
                json_blob(&update.metadata)?,
            ],
        )?;

        let mut budget_reported = None;
        let mut budget_released = None;

        if let Some(cost) = &update.final_cost {
            tx.execute(
                "UPDATE thread_runtime
                 SET provider = ?2,
                     turns = ?3,
                     input_tokens = ?4,
                     output_tokens = ?5,
                     spend = ?6,
                     metadata = ?7
                 WHERE thread_id = ?1",
                params![
                    thread_id,
                    &cost.provider,
                    cost.turns,
                    cost.input_tokens,
                    cost.output_tokens,
                    cost.spend,
                    json_blob(&cost.metadata)?,
                ],
            )?;

            let updated_budget_rows = tx.execute(
                "UPDATE thread_budgets
                 SET actual_spend = ?2,
                     status = COALESCE(?3, status),
                     metadata = COALESCE(?4, metadata)
                 WHERE thread_id = ?1",
                params![
                    thread_id,
                    cost.spend,
                    &update.budget_status,
                    json_blob(&update.budget_metadata)?,
                ],
            )?;
            if updated_budget_rows > 0 {
                budget_reported = Some(json!({
                    "actual_spend": cost.spend,
                }));
                if let Some(status) = &update.budget_status {
                    budget_released = Some(json!({
                        "status": status,
                    }));
                }
            }
        } else if update.budget_status.is_some() || update.budget_metadata.is_some() {
            let updated_budget_rows = tx.execute(
                "UPDATE thread_budgets
                 SET status = COALESCE(?2, status),
                     metadata = COALESCE(?3, metadata)
                 WHERE thread_id = ?1",
                params![
                    thread_id,
                    &update.budget_status,
                    json_blob(&update.budget_metadata)?
                ],
            )?;
            if updated_budget_rows > 0 {
                if let Some(status) = &update.budget_status {
                    budget_released = Some(json!({
                        "status": status,
                    }));
                }
            }
        }

        let mut artifact_events = Vec::with_capacity(update.artifacts.len());
        for artifact in &update.artifacts {
            tx.execute(
                "INSERT INTO thread_artifacts (thread_id, artifact_type, uri, content_hash, metadata)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    thread_id,
                    &artifact.artifact_type,
                    &artifact.uri,
                    &artifact.content_hash,
                    json_blob(&artifact.metadata)?,
                ],
            )?;
            let artifact_id = tx.last_insert_rowid();
            artifact_events.push(NewEventRecord {
                event_type: "artifact_published".to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({
                    "artifact_id": artifact_id,
                    "artifact_type": artifact.artifact_type,
                    "uri": artifact.uri,
                    "content_hash": artifact.content_hash,
                }),
            });
        }

        let mut events_to_append = Vec::with_capacity(
            artifact_events.len()
                + usize::from(budget_reported.is_some())
                + usize::from(budget_released.is_some())
                + 1,
        );
        if let Some(payload) = budget_reported {
            events_to_append.push(NewEventRecord {
                event_type: "budget_reported".to_string(),
                storage_class: "indexed".to_string(),
                payload,
            });
        }
        if let Some(payload) = budget_released {
            events_to_append.push(NewEventRecord {
                event_type: "budget_released".to_string(),
                storage_class: "indexed".to_string(),
                payload,
            });
        }
        events_to_append.extend(artifact_events);
        events_to_append.push(NewEventRecord {
            event_type: terminal_event_type(&update.status)?.to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "outcome_code": update.outcome_code,
                "has_error": update.error_json.is_some(),
                "artifact_count": update.artifacts.len(),
            }),
        });

        let persisted = append_events_tx(&tx, &chain_root_id, thread_id, &events_to_append)?;

        tx.commit()?;
        Ok(persisted)
    }

    pub fn publish_artifact(
        &self,
        thread_id: &str,
        artifact: &NewArtifactRecord,
    ) -> Result<(ThreadArtifactRecord, Vec<PersistedEventRecord>)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (chain_root_id, _) = require_thread_identity(&tx, thread_id)?;

        tx.execute(
            "INSERT INTO thread_artifacts (thread_id, artifact_type, uri, content_hash, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                thread_id,
                &artifact.artifact_type,
                &artifact.uri,
                &artifact.content_hash,
                json_blob(&artifact.metadata)?,
            ],
        )?;
        let artifact_id = tx.last_insert_rowid();

        let persisted = append_events_tx(
            &tx,
            &chain_root_id,
            thread_id,
            &[NewEventRecord {
                event_type: "artifact_published".to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({
                    "artifact_id": artifact_id,
                    "artifact_type": artifact.artifact_type,
                    "uri": artifact.uri,
                    "content_hash": artifact.content_hash,
                }),
            }],
        )?;

        tx.commit()?;

        Ok((
            ThreadArtifactRecord {
                artifact_id,
                artifact_type: artifact.artifact_type.clone(),
                uri: artifact.uri.clone(),
                content_hash: artifact.content_hash.clone(),
                metadata: artifact.metadata.clone(),
            },
            persisted,
        ))
    }

    pub fn append_events(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        events: &[NewEventRecord],
    ) -> Result<Vec<PersistedEventRecord>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let persisted = append_events_tx(&tx, chain_root_id, thread_id, events)?;
        tx.commit()?;
        Ok(persisted)
    }

    pub fn replay_events(
        &self,
        chain_root_id: &str,
        thread_id: Option<&str>,
        after_chain_seq: Option<i64>,
        limit: usize,
    ) -> Result<Vec<PersistedEventRecord>> {
        let conn = self.connect()?;
        let cursor = after_chain_seq.unwrap_or(0);

        let sql_with_thread =
            "SELECT e.event_id, e.chain_root_id, e.chain_seq, e.thread_id, e.thread_seq,
                    e.event_type, e.storage_class, e.ts, e.payload
             FROM event_replay_index AS replay
             INNER JOIN events AS e
               ON e.chain_root_id = replay.chain_root_id
              AND e.chain_seq = replay.chain_seq
             WHERE replay.chain_root_id = ?1
               AND replay.thread_id = ?2
               AND replay.chain_seq > ?3
             ORDER BY replay.chain_seq ASC
             LIMIT ?4";
        let sql_without_thread =
            "SELECT e.event_id, e.chain_root_id, e.chain_seq, e.thread_id, e.thread_seq,
                    e.event_type, e.storage_class, e.ts, e.payload
             FROM event_replay_index AS replay
             INNER JOIN events AS e
               ON e.chain_root_id = replay.chain_root_id
              AND e.chain_seq = replay.chain_seq
             WHERE replay.chain_root_id = ?1
               AND replay.chain_seq > ?2
             ORDER BY replay.chain_seq ASC
             LIMIT ?3";

        let mut events = Vec::new();
        if let Some(thread_id) = thread_id {
            let mut stmt = conn.prepare(sql_with_thread)?;
            let rows = stmt.query_map(
                params![chain_root_id, thread_id, cursor, limit as i64],
                read_event_row,
            )?;
            for row in rows {
                events.push(row?);
            }
        } else {
            let mut stmt = conn.prepare(sql_without_thread)?;
            let rows =
                stmt.query_map(params![chain_root_id, cursor, limit as i64], read_event_row)?;
            for row in rows {
                events.push(row?);
            }
        }

        Ok(events)
    }

    pub fn list_events(
        &self,
        chain_root_id: &str,
        thread_id: Option<&str>,
        after_chain_seq: Option<i64>,
        limit: usize,
    ) -> Result<Vec<PersistedEventRecord>> {
        let conn = self.connect()?;
        let cursor = after_chain_seq.unwrap_or(0);

        let sql_with_thread = "SELECT event_id, chain_root_id, chain_seq, thread_id, thread_seq,
                    event_type, storage_class, ts, payload
             FROM events
             WHERE chain_root_id = ?1
               AND thread_id = ?2
               AND chain_seq > ?3
             ORDER BY chain_seq ASC
             LIMIT ?4";
        let sql_without_thread = "SELECT event_id, chain_root_id, chain_seq, thread_id, thread_seq,
                    event_type, storage_class, ts, payload
             FROM events
             WHERE chain_root_id = ?1
               AND chain_seq > ?2
             ORDER BY chain_seq ASC
             LIMIT ?3";

        let mut events = Vec::new();
        if let Some(thread_id) = thread_id {
            let mut stmt = conn.prepare(sql_with_thread)?;
            let rows = stmt.query_map(
                params![chain_root_id, thread_id, cursor, limit as i64],
                read_event_row,
            )?;
            for row in rows {
                events.push(row?);
            }
        } else {
            let mut stmt = conn.prepare(sql_without_thread)?;
            let rows =
                stmt.query_map(params![chain_root_id, cursor, limit as i64], read_event_row)?;
            for row in rows {
                events.push(row?);
            }
        }

        Ok(events)
    }

    pub fn get_budget(&self, thread_id: &str) -> Result<Option<BudgetInfo>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT budget_parent_id, reserved_spend, actual_spend, status, metadata
             FROM thread_budgets WHERE thread_id = ?1",
            params![thread_id],
            |row| {
                Ok(BudgetInfo {
                    budget_parent_id: row.get(0)?,
                    reserved_spend: row.get(1)?,
                    actual_spend: row.get(2)?,
                    status: row.get(3)?,
                    metadata: parse_json_blob(row.get(4)?)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn reserve_budget(
        &self,
        thread_id: &str,
        budget_parent_id: &str,
        reserved_spend: f64,
        metadata: Option<&Value>,
    ) -> Result<(BudgetInfo, Vec<PersistedEventRecord>)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (chain_root_id, _) = require_thread_identity(&tx, thread_id)?;

        let parent_metadata = tx
            .query_row(
                "SELECT metadata FROM thread_budgets WHERE thread_id = ?1",
                params![budget_parent_id],
                |row| row.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("budget parent not found: {budget_parent_id}"))?;
        let parent_metadata = parse_json_blob(parent_metadata)?;
        if let Some(max_spend) = parent_metadata
            .as_ref()
            .and_then(|value| value.get("max_spend"))
            .and_then(Value::as_f64)
        {
            let already_reserved: f64 = tx.query_row(
                "SELECT COALESCE(SUM(reserved_spend), 0.0)
                 FROM thread_budgets
                 WHERE budget_parent_id = ?1 AND status = 'open'",
                params![budget_parent_id],
                |row| row.get(0),
            )?;
            if already_reserved + reserved_spend > max_spend + f64::EPSILON {
                bail!(
                    "budget reserve exceeds parent max_spend: requested={} reserved={} max_spend={}",
                    reserved_spend,
                    already_reserved,
                    max_spend
                );
            }
        }

        tx.execute(
            "INSERT INTO thread_budgets (
                thread_id, budget_parent_id, reserved_spend, actual_spend, status, metadata
             ) VALUES (?1, ?2, ?3, 0.0, 'open', ?4)",
            params![
                thread_id,
                budget_parent_id,
                reserved_spend,
                json_blob_ref(metadata)?
            ],
        )?;

        let budget = load_budget_in_tx(&tx, thread_id)?;
        let persisted = append_events_tx(
            &tx,
            &chain_root_id,
            thread_id,
            &[NewEventRecord {
                event_type: "budget_reserved".to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({
                    "budget_parent_id": budget_parent_id,
                    "reserved_spend": reserved_spend,
                }),
            }],
        )?;

        tx.commit()?;
        Ok((budget, persisted))
    }

    pub fn report_budget(
        &self,
        thread_id: &str,
        actual_spend: f64,
        metadata: Option<&Value>,
    ) -> Result<(BudgetInfo, Vec<PersistedEventRecord>)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (chain_root_id, _) = require_thread_identity(&tx, thread_id)?;

        let updated = tx.execute(
            "UPDATE thread_budgets
             SET actual_spend = ?2,
                 metadata = COALESCE(?3, metadata)
             WHERE thread_id = ?1",
            params![thread_id, actual_spend, json_blob_ref(metadata)?],
        )?;
        if updated == 0 {
            bail!("budget not found for thread: {thread_id}");
        }

        tx.execute(
            "UPDATE thread_runtime SET spend = ?2 WHERE thread_id = ?1",
            params![thread_id, actual_spend],
        )?;

        let budget = load_budget_in_tx(&tx, thread_id)?;
        let persisted = append_events_tx(
            &tx,
            &chain_root_id,
            thread_id,
            &[NewEventRecord {
                event_type: "budget_reported".to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({
                    "actual_spend": actual_spend,
                }),
            }],
        )?;

        tx.commit()?;
        Ok((budget, persisted))
    }

    pub fn release_budget(
        &self,
        thread_id: &str,
        status: &str,
        metadata: Option<&Value>,
    ) -> Result<(BudgetInfo, Vec<PersistedEventRecord>)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (chain_root_id, _) = require_thread_identity(&tx, thread_id)?;

        let updated = tx.execute(
            "UPDATE thread_budgets
             SET status = ?2,
                 metadata = COALESCE(?3, metadata)
             WHERE thread_id = ?1",
            params![thread_id, status, json_blob_ref(metadata)?],
        )?;
        if updated == 0 {
            bail!("budget not found for thread: {thread_id}");
        }

        let budget = load_budget_in_tx(&tx, thread_id)?;
        let persisted = append_events_tx(
            &tx,
            &chain_root_id,
            thread_id,
            &[NewEventRecord {
                event_type: "budget_released".to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({
                    "status": status,
                }),
            }],
        )?;

        tx.commit()?;
        Ok((budget, persisted))
    }

    pub fn submit_command(
        &self,
        command: &NewCommandRecord,
    ) -> Result<(CommandRecord, Vec<PersistedEventRecord>)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (chain_root_id, _) = require_thread_identity(&tx, &command.thread_id)?;
        let now = now_rfc3339();
        tx.execute(
            "INSERT INTO thread_commands (
                thread_id, command_type, status, requested_by, params, result,
                created_at, claimed_at, completed_at
             ) VALUES (?1, ?2, 'pending', ?3, ?4, NULL, ?5, NULL, NULL)",
            params![
                &command.thread_id,
                &command.command_type,
                &command.requested_by,
                json_blob(&command.params)?,
                now,
            ],
        )?;

        let command_id = tx.last_insert_rowid();
        let command = load_command_in_tx(&tx, command_id)?;
        let persisted = append_events_tx(
            &tx,
            &chain_root_id,
            &command.thread_id,
            &[NewEventRecord {
                event_type: "command_submitted".to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({
                    "command_id": command.command_id,
                    "command_type": command.command_type,
                }),
            }],
        )?;

        tx.commit()?;
        Ok((command, persisted))
    }

    pub fn claim_commands(
        &self,
        thread_id: &str,
    ) -> Result<(Vec<CommandRecord>, Vec<PersistedEventRecord>)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (chain_root_id, _) = require_thread_identity(&tx, thread_id)?;
        let now = now_rfc3339();

        let mut stmt = tx.prepare(
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
            tx.execute(
                "UPDATE thread_commands SET status = 'claimed', claimed_at = ?2 WHERE command_id = ?1",
                params![command.command_id, now],
            )?;
            command.status = "claimed".to_string();
            command.claimed_at = Some(now.clone());
            commands.push(command);
        }

        drop(stmt);
        let persisted = if commands.is_empty() {
            Vec::new()
        } else {
            let events = commands
                .iter()
                .map(|command| NewEventRecord {
                    event_type: "command_claimed".to_string(),
                    storage_class: "indexed".to_string(),
                    payload: json!({
                        "command_id": command.command_id,
                        "command_type": command.command_type,
                    }),
                })
                .collect::<Vec<_>>();
            append_events_tx(&tx, &chain_root_id, thread_id, &events)?
        };
        tx.commit()?;
        Ok((commands, persisted))
    }

    pub fn complete_command(
        &self,
        command_id: i64,
        status: &str,
        result: Option<&Value>,
    ) -> Result<(CommandRecord, Vec<PersistedEventRecord>)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated = tx.execute(
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

        let command = load_command_in_tx(&tx, command_id)?;
        let (chain_root_id, _) = require_thread_identity(&tx, &command.thread_id)?;
        let persisted = append_events_tx(
            &tx,
            &chain_root_id,
            &command.thread_id,
            &[NewEventRecord {
                event_type: "command_completed".to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({
                    "command_id": command.command_id,
                    "command_type": command.command_type,
                    "status": command.status,
                }),
            }],
        )?;

        tx.commit()?;
        Ok((command, persisted))
    }

    pub fn get_thread_result(&self, thread_id: &str) -> Result<Option<ThreadResultRecord>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT outcome_code, result_json, error_json, metadata
             FROM thread_results WHERE thread_id = ?1",
            params![thread_id],
            |row| {
                Ok(ThreadResultRecord {
                    outcome_code: row.get(0)?,
                    result: parse_json_blob(row.get(1)?)?,
                    error: parse_json_blob(row.get(2)?)?,
                    metadata: parse_json_blob(row.get(3)?)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_thread_artifacts(&self, thread_id: &str) -> Result<Vec<ThreadArtifactRecord>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT artifact_id, artifact_type, uri, content_hash, metadata
             FROM thread_artifacts
             WHERE thread_id = ?1
             ORDER BY artifact_id ASC",
        )?;
        let rows = stmt.query_map(params![thread_id], |row| {
            Ok(ThreadArtifactRecord {
                artifact_id: row.get(0)?,
                artifact_type: row.get(1)?,
                uri: row.get(2)?,
                content_hash: row.get(3)?,
                metadata: parse_json_blob(row.get(4)?)?,
            })
        })?;

        let mut artifacts = Vec::new();
        for row in rows {
            artifacts.push(row?);
        }
        Ok(artifacts)
    }

    pub fn set_facets(
        &self,
        thread_id: &str,
        facets: &[(String, String)],
    ) -> Result<()> {
        let conn = self.connect()?;
        for (key, value) in facets {
            conn.execute(
                "INSERT INTO thread_facets (thread_id, facet_key, facet_value)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(thread_id, facet_key) DO UPDATE SET facet_value = excluded.facet_value",
                params![thread_id, key, value],
            )?;
        }
        Ok(())
    }

    pub fn get_facets(&self, thread_id: &str) -> Result<Vec<(String, String)>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT facet_key, facet_value FROM thread_facets WHERE thread_id = ?1 ORDER BY facet_key",
        )?;
        let rows = stmt.query_map(params![thread_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut facets = Vec::new();
        for row in rows {
            facets.push(row?);
        }
        Ok(facets)
    }

    pub fn list_thread_children(&self, thread_id: &str) -> Result<Vec<ThreadDetail>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT target_thread_id
             FROM thread_edges
             WHERE source_thread_id = ?1 AND edge_type = 'spawned'
             ORDER BY edge_id ASC",
        )?;
        let rows = stmt.query_map(params![thread_id], |row| row.get::<_, String>(0))?;

        let mut children = Vec::new();
        for row in rows {
            if let Some(thread) = self.get_thread(&row?)? {
                children.push(thread);
            }
        }
        Ok(children)
    }

    pub fn list_chain_threads(&self, chain_root_id: &str) -> Result<Vec<ThreadDetail>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT thread_id
             FROM threads
             WHERE chain_root_id = ?1
             ORDER BY created_at ASC, thread_id ASC",
        )?;
        let rows = stmt.query_map(params![chain_root_id], |row| row.get::<_, String>(0))?;

        let mut threads = Vec::new();
        for row in rows {
            if let Some(thread) = self.get_thread(&row?)? {
                threads.push(thread);
            }
        }
        Ok(threads)
    }

    pub fn list_chain_edges(&self, chain_root_id: &str) -> Result<Vec<ThreadEdgeRecord>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT edge_id, chain_root_id, source_thread_id, target_thread_id,
                    edge_type, created_at, metadata
             FROM thread_edges
             WHERE chain_root_id = ?1
             ORDER BY edge_id ASC",
        )?;
        let rows = stmt.query_map(params![chain_root_id], read_thread_edge_row)?;

        let mut edges = Vec::new();
        for row in rows {
            edges.push(row?);
        }
        Ok(edges)
    }

    pub fn active_thread_count(&self) -> Result<i64> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT COUNT(*) FROM threads WHERE status IN ('created', 'running')",
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.connect()?;
        conn.execute_batch(MIGRATIONS)
            .context("failed to initialize ryeosd database schema")?;
        Ok(())
    }

    fn connect(&self) -> Result<Connection> {
        let conn = Connection::open(&self.path)
            .with_context(|| format!("failed to open database {}", self.path.display()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;
        Ok(conn)
    }
}

fn derive_allowed_actions(
    profile: Option<&crate::kind_profiles::ThreadKindProfile>,
    status: &str,
    has_process: bool,
) -> Vec<String> {
    let Some(profile) = profile else {
        return Vec::new();
    };

    match status {
        "created" | "running" => {
            let mut actions = vec!["cancel".to_string()];
            if has_process {
                actions.push("kill".to_string());
            }
            if profile.supports_interrupt {
                actions.push("interrupt".to_string());
            }
            actions
        }
        "completed" | "failed" | "cancelled" | "killed" | "timed_out" => {
            if profile.supports_continuation {
                vec!["continue".to_string()]
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

fn load_budget_in_tx(tx: &Transaction<'_>, thread_id: &str) -> Result<BudgetInfo> {
    tx.query_row(
        "SELECT budget_parent_id, reserved_spend, actual_spend, status, metadata
         FROM thread_budgets WHERE thread_id = ?1",
        params![thread_id],
        |row| {
            Ok(BudgetInfo {
                budget_parent_id: row.get(0)?,
                reserved_spend: row.get(1)?,
                actual_spend: row.get(2)?,
                status: row.get(3)?,
                metadata: parse_json_blob(row.get(4)?)?,
            })
        },
    )
    .map_err(Into::into)
}

fn load_command_in_tx(tx: &Transaction<'_>, command_id: i64) -> Result<CommandRecord> {
    tx.query_row(
        "SELECT command_id, thread_id, command_type, status, requested_by, params,
                result, created_at, claimed_at, completed_at
         FROM thread_commands
         WHERE command_id = ?1",
        params![command_id],
        read_command_row,
    )
    .optional()?
    .ok_or_else(|| anyhow!("command missing from database: {command_id}"))
}

fn read_event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PersistedEventRecord> {
    Ok(PersistedEventRecord {
        event_id: row.get(0)?,
        chain_root_id: row.get(1)?,
        chain_seq: row.get(2)?,
        thread_id: row.get(3)?,
        thread_seq: row.get(4)?,
        event_type: row.get(5)?,
        storage_class: row.get(6)?,
        ts: row.get(7)?,
        payload: parse_json_blob(row.get(8)?)?.unwrap_or(Value::Object(Default::default())),
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

fn read_thread_edge_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ThreadEdgeRecord> {
    Ok(ThreadEdgeRecord {
        edge_id: row.get(0)?,
        chain_root_id: row.get(1)?,
        source_thread_id: row.get(2)?,
        target_thread_id: row.get(3)?,
        edge_type: row.get(4)?,
        created_at: row.get(5)?,
        metadata: parse_json_blob(row.get(6)?)?,
    })
}

fn append_events_tx(
    tx: &Transaction<'_>,
    chain_root_id: &str,
    thread_id: &str,
    events: &[NewEventRecord],
) -> Result<Vec<PersistedEventRecord>> {
    if events.is_empty() {
        return Ok(Vec::new());
    }

    let mut next_chain_seq = next_chain_seq(tx, chain_root_id)?;
    let mut next_thread_seq = next_thread_seq(tx, thread_id)?;
    let mut persisted = Vec::with_capacity(events.len());

    for event in events {
        let ts = now_rfc3339();
        tx.execute(
            "INSERT INTO events (
                chain_root_id, chain_seq, thread_id, thread_seq, event_type,
                storage_class, ts, payload
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                chain_root_id,
                next_chain_seq,
                thread_id,
                next_thread_seq,
                &event.event_type,
                &event.storage_class,
                &ts,
                serde_json::to_vec(&event.payload).context("failed to encode event payload")?,
            ],
        )?;
        let event_id = tx.last_insert_rowid();

        if event.storage_class == "indexed" {
            tx.execute(
                "INSERT INTO event_replay_index (
                    chain_root_id, chain_seq, thread_id, event_type, ts, payload
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    chain_root_id,
                    next_chain_seq,
                    thread_id,
                    &event.event_type,
                    &ts,
                    serde_json::to_vec(&event.payload)
                        .context("failed to encode replay payload")?,
                ],
            )?;
        }

        persisted.push(PersistedEventRecord {
            event_id,
            chain_root_id: chain_root_id.to_string(),
            chain_seq: next_chain_seq,
            thread_id: thread_id.to_string(),
            thread_seq: next_thread_seq,
            event_type: event.event_type.clone(),
            storage_class: event.storage_class.clone(),
            ts,
            payload: event.payload.clone(),
        });

        next_chain_seq += 1;
        next_thread_seq += 1;
    }

    tx.execute(
        "INSERT INTO chain_counters (chain_root_id, next_chain_seq)
         VALUES (?1, ?2)
         ON CONFLICT(chain_root_id) DO UPDATE SET next_chain_seq = excluded.next_chain_seq",
        params![chain_root_id, next_chain_seq],
    )?;
    tx.execute(
        "INSERT INTO thread_counters (thread_id, next_thread_seq)
         VALUES (?1, ?2)
         ON CONFLICT(thread_id) DO UPDATE SET next_thread_seq = excluded.next_thread_seq",
        params![thread_id, next_thread_seq],
    )?;

    Ok(persisted)
}

fn next_chain_seq(tx: &Transaction<'_>, chain_root_id: &str) -> Result<i64> {
    tx.query_row(
        "SELECT next_chain_seq FROM chain_counters WHERE chain_root_id = ?1",
        params![chain_root_id],
        |row| row.get(0),
    )
    .optional()
    .map(|value| value.unwrap_or(1))
    .map_err(Into::into)
}

fn next_thread_seq(tx: &Transaction<'_>, thread_id: &str) -> Result<i64> {
    tx.query_row(
        "SELECT next_thread_seq FROM thread_counters WHERE thread_id = ?1",
        params![thread_id],
        |row| row.get(0),
    )
    .optional()
    .map(|value| value.unwrap_or(1))
    .map_err(Into::into)
}

fn require_thread_identity(tx: &Transaction<'_>, thread_id: &str) -> Result<(String, String)> {
    tx.query_row(
        "SELECT chain_root_id, status FROM threads WHERE thread_id = ?1",
        params![thread_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()?
    .ok_or_else(|| anyhow::anyhow!("thread not found: {thread_id}"))
}

fn is_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "cancelled" | "killed" | "timed_out" | "continued"
    )
}

fn terminal_event_type(status: &str) -> Result<&'static str> {
    match status {
        "completed" => Ok("thread_completed"),
        "failed" => Ok("thread_failed"),
        "cancelled" => Ok("thread_cancelled"),
        "killed" => Ok("thread_killed"),
        "timed_out" => Ok("thread_timed_out"),
        "continued" => Ok("thread_continued"),
        other => bail!("invalid terminal event status: {other}"),
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
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
