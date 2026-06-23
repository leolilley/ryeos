//! Query layer for the ryeos-state projection database.
//!
//! Provides typed read accessors for all projection tables plus
//! a small write helper for thread facets.

use anyhow::Context;
use rusqlite::OptionalExtension;

use crate::projection::ProjectionDb;

// ============= Row types =============

#[derive(Debug, Clone)]
pub struct ThreadRow {
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
}

impl ThreadRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            thread_id: row.get("thread_id")?,
            chain_root_id: row.get("chain_root_id")?,
            kind: row.get("kind")?,
            status: row.get("status")?,
            item_ref: row.get("item_ref")?,
            executor_ref: row.get("executor_ref")?,
            launch_mode: row.get("launch_mode")?,
            current_site_id: row.get("current_site_id")?,
            origin_site_id: row.get("origin_site_id")?,
            upstream_thread_id: row.get("upstream_thread_id")?,
            requested_by: row.get("requested_by")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
            started_at: row.get("started_at")?,
            finished_at: row.get("finished_at")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct EventRow {
    pub event_id: i64,
    pub chain_root_id: String,
    pub chain_seq: i64,
    pub thread_id: String,
    pub thread_seq: i64,
    pub event_type: String,
    pub durability: String,
    pub ts: String,
    pub prev_chain_event_hash: Option<String>,
    pub prev_thread_event_hash: Option<String>,
    pub payload: Vec<u8>,
}

impl EventRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            event_id: row.get("event_id")?,
            chain_root_id: row.get("chain_root_id")?,
            chain_seq: row.get("chain_seq")?,
            thread_id: row.get("thread_id")?,
            thread_seq: row.get("thread_seq")?,
            event_type: row.get("event_type")?,
            durability: row.get("durability")?,
            ts: row.get("ts")?,
            prev_chain_event_hash: row.get("prev_chain_event_hash")?,
            prev_thread_event_hash: row.get("prev_thread_event_hash")?,
            payload: row.get("payload")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ThreadResultRow {
    pub thread_id: String,
    pub chain_root_id: String,
    pub status: String,
    pub result: Option<Vec<u8>>,
    pub outcome_code: Option<String>,
    pub error: Option<String>,
    pub updated_at: String,
}

impl ThreadResultRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            thread_id: row.get("thread_id")?,
            chain_root_id: row.get("chain_root_id")?,
            status: row.get("status")?,
            result: row.get("result")?,
            outcome_code: row.get("outcome_code")?,
            error: row.get("error")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ThreadEdgeRow {
    pub chain_root_id: String,
    pub parent_thread_id: String,
    pub child_thread_id: String,
    pub spawn_seq: Option<i64>,
    pub spawn_reason: Option<String>,
}

impl ThreadEdgeRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            chain_root_id: row.get("chain_root_id")?,
            parent_thread_id: row.get("parent_thread_id")?,
            child_thread_id: row.get("child_thread_id")?,
            spawn_seq: row.get("spawn_seq")?,
            spawn_reason: row.get("spawn_reason")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactRow {
    pub chain_root_id: String,
    pub thread_id: String,
    pub kind: String,
    pub metadata: Option<Vec<u8>>,
    pub created_at: String,
}

impl ArtifactRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            chain_root_id: row.get("chain_root_id")?,
            thread_id: row.get("thread_id")?,
            kind: row.get("kind")?,
            metadata: row.get("metadata")?,
            created_at: row.get("created_at")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct FacetRow {
    pub thread_id: String,
    pub key: String,
    pub value: Vec<u8>,
}

impl FacetRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            thread_id: row.get("thread_id")?,
            key: row.get("key")?,
            value: row.get("value")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ThreadUsageLatestRow {
    pub thread_id: String,
    pub chain_root_id: String,
    pub chain_seq: i64,
    pub thread_seq: i64,
    pub completed_turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub spend_usd: f64,
    pub spawns_used: i64,
    pub started_at: String,
    pub settled_at: String,
    pub last_settled_turn_seq: i64,
    pub elapsed_ms: i64,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub profile: Option<String>,
}

impl ThreadUsageLatestRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            thread_id: row.get("thread_id")?,
            chain_root_id: row.get("chain_root_id")?,
            chain_seq: row.get("chain_seq")?,
            thread_seq: row.get("thread_seq")?,
            completed_turns: row.get("completed_turns")?,
            input_tokens: row.get("input_tokens")?,
            output_tokens: row.get("output_tokens")?,
            spend_usd: row.get("spend_usd")?,
            spawns_used: row.get("spawns_used")?,
            started_at: row.get("started_at")?,
            settled_at: row.get("settled_at")?,
            last_settled_turn_seq: row.get("last_settled_turn_seq")?,
            elapsed_ms: row.get("elapsed_ms")?,
            provider_id: row.get("provider_id")?,
            model: row.get("model")?,
            profile: row.get("profile")?,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThreadUsageTotals {
    pub thread_count: i64,
    pub completed_turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub spend_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadUsageSubjectRow {
    pub chain_root_id: String,
    pub namespace: String,
    pub subject: String,
    pub asserted_by: Option<String>,
    pub created_at: String,
}

impl ThreadUsageSubjectRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            chain_root_id: row.get("chain_root_id")?,
            namespace: row.get("namespace")?,
            subject: row.get("subject")?,
            asserted_by: row.get("asserted_by")?,
            created_at: row.get("created_at")?,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct UsageSummaryFilter<'a> {
    pub namespace: Option<&'a str>,
    pub subject: Option<&'a str>,
    pub asserted_by: Option<&'a str>,
    pub settled_at_gte: Option<&'a str>,
    pub settled_at_lt: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsageSummaryRow {
    pub namespace: String,
    pub subject: String,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub profile: Option<String>,
    pub chain_count: i64,
    pub thread_count: i64,
    pub completed_turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub spend_usd: f64,
}

impl UsageSummaryRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            namespace: row.get("namespace")?,
            subject: row.get("subject")?,
            provider_id: row.get("provider_id")?,
            model: row.get("model")?,
            profile: row.get("profile")?,
            chain_count: row.get("chain_count")?,
            thread_count: row.get("thread_count")?,
            completed_turns: row.get("completed_turns")?,
            input_tokens: row.get("input_tokens")?,
            output_tokens: row.get("output_tokens")?,
            spend_usd: row.get("spend_usd")?,
        })
    }
}

// ============= Query functions =============

const THREAD_COLUMNS: &str = r#"
    thread_id, chain_root_id, kind, status,
    item_ref, executor_ref, launch_mode,
    current_site_id, origin_site_id, upstream_thread_id, requested_by,
    created_at, updated_at, started_at, finished_at
"#;

pub fn get_thread(db: &ProjectionDb, thread_id: &str) -> anyhow::Result<Option<ThreadRow>> {
    let sql = &format!("SELECT {THREAD_COLUMNS} FROM threads WHERE thread_id = ?");
    let mut stmt = db.connection().prepare(sql).context("prepare get_thread")?;
    stmt.query_row([thread_id], ThreadRow::from_row)
        .optional()
        .context("query get_thread")
}

pub fn list_threads_by_chain(
    db: &ProjectionDb,
    chain_root_id: &str,
) -> anyhow::Result<Vec<ThreadRow>> {
    let sql = &format!(
        "SELECT {THREAD_COLUMNS} FROM threads WHERE chain_root_id = ? ORDER BY created_at"
    );
    let mut stmt = db
        .connection()
        .prepare(sql)
        .context("prepare list_threads_by_chain")?;
    let rows = stmt
        .query_map([chain_root_id], ThreadRow::from_row)
        .context("query list_threads_by_chain")?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub fn list_threads_by_status(
    db: &ProjectionDb,
    statuses: &[&str],
) -> anyhow::Result<Vec<ThreadRow>> {
    if statuses.is_empty() {
        return Ok(vec![]);
    }
    let placeholders: Vec<&str> = statuses.iter().map(|_| "?").collect();
    let sql = &format!(
        "SELECT {THREAD_COLUMNS} FROM threads WHERE status IN ({}) ORDER BY created_at",
        placeholders.join(", ")
    );
    let mut stmt = db
        .connection()
        .prepare(sql)
        .context("prepare list_threads_by_status")?;
    let params: Vec<&dyn rusqlite::types::ToSql> = statuses
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let rows = stmt
        .query_map(params.as_slice(), ThreadRow::from_row)
        .context("query list_threads_by_status")?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub fn list_threads(db: &ProjectionDb, limit: usize) -> anyhow::Result<Vec<ThreadRow>> {
    let sql = &format!("SELECT {THREAD_COLUMNS} FROM threads ORDER BY created_at LIMIT ?");
    let mut stmt = db
        .connection()
        .prepare(sql)
        .context("prepare list_threads")?;
    let rows = stmt
        .query_map([limit], ThreadRow::from_row)
        .context("query list_threads")?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// List threads with optional principal filtering.
///
/// When `filter_principal` is `Some(fp)`, only threads with
/// `requested_by = fp` are returned. `None` returns all threads.
pub fn list_threads_filtered(
    db: &ProjectionDb,
    limit: usize,
    filter_principal: Option<&str>,
) -> anyhow::Result<Vec<ThreadRow>> {
    let sql = match filter_principal {
        Some(_) => format!(
            "SELECT {THREAD_COLUMNS} FROM threads WHERE requested_by = ? ORDER BY created_at LIMIT ?"
        ),
        None => format!(
            "SELECT {THREAD_COLUMNS} FROM threads ORDER BY created_at LIMIT ?"
        ),
    };
    let mut stmt = db
        .connection()
        .prepare(&sql)
        .context("prepare list_threads_filtered")?;
    let rows = match filter_principal {
        Some(fp) => {
            let params: [&dyn rusqlite::types::ToSql; 2] = [&fp, &limit];
            stmt.query_map(params, ThreadRow::from_row)
                .context("query list_threads_filtered")?
        }
        None => {
            let params: [&dyn rusqlite::types::ToSql; 1] = [&limit];
            stmt.query_map(params, ThreadRow::from_row)
                .context("query list_threads_filtered")?
        }
    };
    Ok(rows.filter_map(|r| r.ok()).collect())
}

const TERMINAL_STATUSES: [&str; 6] = [
    "completed",
    "failed",
    "cancelled",
    "killed",
    "timed_out",
    "continued",
];

pub fn active_thread_count(db: &ProjectionDb) -> anyhow::Result<i64> {
    let placeholders: Vec<&str> = TERMINAL_STATUSES.iter().map(|_| "?").collect();
    let sql = &format!(
        "SELECT COUNT(*) FROM threads WHERE status NOT IN ({})",
        placeholders.join(", ")
    );
    let params: Vec<&dyn rusqlite::types::ToSql> = TERMINAL_STATUSES
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let count: i64 = db
        .connection()
        .query_row(sql, params.as_slice(), |row| row.get(0))
        .context("query active_thread_count")?;
    Ok(count)
}

pub fn get_thread_result(
    db: &ProjectionDb,
    thread_id: &str,
) -> anyhow::Result<Option<ThreadResultRow>> {
    let mut stmt = db
        .connection()
        .prepare(
            "SELECT thread_id, chain_root_id, status, result, outcome_code, error, updated_at \
             FROM thread_results WHERE thread_id = ?",
        )
        .context("prepare get_thread_result")?;
    stmt.query_row([thread_id], ThreadResultRow::from_row)
        .optional()
        .context("query get_thread_result")
}

pub fn list_thread_artifacts(
    db: &ProjectionDb,
    thread_id: &str,
) -> anyhow::Result<Vec<ArtifactRow>> {
    let mut stmt = db
        .connection()
        .prepare(
            "SELECT chain_root_id, thread_id, kind, metadata, created_at \
             FROM thread_artifacts WHERE thread_id = ? ORDER BY created_at",
        )
        .context("prepare list_thread_artifacts")?;
    let rows = stmt
        .query_map([thread_id], ArtifactRow::from_row)
        .context("query list_thread_artifacts")?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub fn list_thread_children(db: &ProjectionDb, parent_id: &str) -> anyhow::Result<Vec<ThreadRow>> {
    let sql = &format!(
        "SELECT {THREAD_COLUMNS} FROM threads \
         WHERE thread_id IN (\
             SELECT child_thread_id FROM thread_edges WHERE parent_thread_id = ?\
         ) ORDER BY created_at"
    );
    let mut stmt = db
        .connection()
        .prepare(sql)
        .context("prepare list_thread_children")?;
    let rows = stmt
        .query_map([parent_id], ThreadRow::from_row)
        .context("query list_thread_children")?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub fn list_thread_edges(
    db: &ProjectionDb,
    chain_root_id: &str,
) -> anyhow::Result<Vec<ThreadEdgeRow>> {
    let mut stmt = db
        .connection()
        .prepare(
            "SELECT chain_root_id, parent_thread_id, child_thread_id, spawn_seq, spawn_reason \
             FROM thread_edges WHERE chain_root_id = ? ORDER BY spawn_seq IS NULL, spawn_seq, rowid",
        )
        .context("prepare list_thread_edges")?;
    let rows = stmt
        .query_map([chain_root_id], ThreadEdgeRow::from_row)
        .context("query list_thread_edges")?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub fn replay_events(
    db: &ProjectionDb,
    chain_root_id: &str,
    thread_id: Option<&str>,
    after_seq: Option<i64>,
    limit: usize,
) -> anyhow::Result<Vec<EventRow>> {
    let mut sql = String::from(
        "SELECT event_id, chain_root_id, chain_seq, thread_id, thread_seq, \
                event_type, durability, ts, prev_chain_event_hash, \
                prev_thread_event_hash, payload \
         FROM events WHERE chain_root_id = ?",
    );
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> =
        vec![Box::new(chain_root_id.to_string())];

    if let Some(tid) = thread_id {
        sql.push_str(" AND thread_id = ?");
        params.push(Box::new(tid.to_string()));
    }
    if let Some(seq) = after_seq {
        sql.push_str(" AND chain_seq > ?");
        params.push(Box::new(seq));
    }

    sql.push_str(" ORDER BY chain_seq LIMIT ?");
    params.push(Box::new(limit as i64));

    let mut stmt = db
        .connection()
        .prepare(&sql)
        .context("prepare replay_events")?;

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt
        .query_map(param_refs.as_slice(), EventRow::from_row)
        .context("query replay_events")?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// The thread that owns the chain's highest-`chain_seq` event — the thread a
/// live tail should currently follow. `chain_seq` is monotonic within a chain,
/// so this is collision-free (unlike ordering threads by `created_at`).
/// Returns `None` when the chain has no events yet.
pub fn chain_head_thread(db: &ProjectionDb, chain_root_id: &str) -> anyhow::Result<Option<String>> {
    db.connection()
        .query_row(
            "SELECT thread_id FROM events WHERE chain_root_id = ? \
             ORDER BY chain_seq DESC LIMIT 1",
            [chain_root_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .context("query chain_head_thread")
}

/// The continuation successor of `thread_id`, if one exists. Reads the
/// `thread_continued` event payload (`{ successor_thread_id, reason }`) written
/// in the same transaction that creates the successor. Authoritative — unlike
/// `thread_edges`, whose `upstream_thread_id`-derived edges do not distinguish a
/// continuation successor from a compose-context child. Returns `None` for a
/// thread that has not been continued.
pub fn continuation_successor(
    db: &ProjectionDb,
    thread_id: &str,
) -> anyhow::Result<Option<String>> {
    let payload: Option<Vec<u8>> = db
        .connection()
        .query_row(
            "SELECT payload FROM events \
             WHERE thread_id = ? AND event_type = 'thread_continued' \
             ORDER BY chain_seq DESC LIMIT 1",
            [thread_id],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .context("query continuation_successor")?;
    let Some(bytes) = payload else {
        return Ok(None);
    };
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).context("parse thread_continued payload")?;
    Ok(value
        .get("successor_thread_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()))
}

pub fn latest_thread_events(
    db: &ProjectionDb,
    thread_id: &str,
    limit: usize,
) -> anyhow::Result<Vec<EventRow>> {
    let mut stmt = db
        .connection()
        .prepare(
            "SELECT event_id, chain_root_id, chain_seq, thread_id, thread_seq, \
                event_type, durability, ts, prev_chain_event_hash, \
                prev_thread_event_hash, payload \
         FROM events WHERE thread_id = ? ORDER BY chain_seq DESC LIMIT ?",
        )
        .context("prepare latest_thread_events")?;
    let rows = stmt
        .query_map((thread_id, limit as i64), EventRow::from_row)
        .context("query latest_thread_events")?;
    let mut events: Vec<EventRow> = rows.filter_map(|r| r.ok()).collect();
    events.reverse();
    Ok(events)
}

pub fn get_facets(db: &ProjectionDb, thread_id: &str) -> anyhow::Result<Vec<FacetRow>> {
    let mut stmt = db
        .connection()
        .prepare(
            "SELECT thread_id, key, value \
             FROM thread_facets WHERE thread_id = ? ORDER BY key",
        )
        .context("prepare get_facets")?;
    let rows = stmt
        .query_map([thread_id], FacetRow::from_row)
        .context("query get_facets")?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

const THREAD_USAGE_LATEST_COLUMNS: &str = r#"
    thread_id, chain_root_id, chain_seq, thread_seq,
    completed_turns, input_tokens, output_tokens, spend_usd, spawns_used,
    started_at, settled_at, last_settled_turn_seq, elapsed_ms,
    provider_id, model, profile
"#;

const EFFECTIVE_USAGE_CTE: &str = r#"
    WITH RECURSIVE
    continuation_edges(source_thread_id, successor_thread_id, chain_root_id) AS (
        SELECT
            e.thread_id AS source_thread_id,
            json_extract(CAST(e.payload AS TEXT), '$.successor_thread_id') AS successor_thread_id,
            e.chain_root_id AS chain_root_id
        FROM events e
        JOIN threads src
          ON src.thread_id = e.thread_id
         AND src.chain_root_id = e.chain_root_id
        -- NB: do NOT gate on src.status = 'continued'. An operator follow-up
        -- preserves a completed/failed predecessor's status, yet its successor's
        -- usage is cumulative; gating on `continued` would drop that edge and
        -- double-count the predecessor. The `thread_continued` event plus the
        -- successor's `upstream_thread_id` linkage below already prove the edge.
        JOIN threads succ
          ON succ.thread_id = json_extract(CAST(e.payload AS TEXT), '$.successor_thread_id')
         AND succ.chain_root_id = e.chain_root_id
         AND succ.upstream_thread_id = e.thread_id
        WHERE e.event_type = 'thread_continued'
          AND json_extract(CAST(e.payload AS TEXT), '$.successor_thread_id') IS NOT NULL
    ),
    continuation_paths(source_thread_id, descendant_thread_id, chain_root_id) AS (
        SELECT source_thread_id, successor_thread_id, chain_root_id
        FROM continuation_edges

        UNION

        SELECT
            p.source_thread_id,
            ce.successor_thread_id,
            p.chain_root_id
        FROM continuation_paths p
        JOIN continuation_edges ce
          ON ce.source_thread_id = p.descendant_thread_id
         AND ce.chain_root_id = p.chain_root_id
    ),
    effective_usage AS (
        SELECT u.*
        FROM thread_usage_latest u
        WHERE NOT EXISTS (
            SELECT 1
            FROM continuation_paths p
            JOIN thread_usage_latest downstream
              ON downstream.thread_id = p.descendant_thread_id
             AND downstream.chain_root_id = p.chain_root_id
            WHERE p.source_thread_id = u.thread_id
              AND p.chain_root_id = u.chain_root_id
        )
    )
"#;

pub fn get_thread_usage_latest(
    db: &ProjectionDb,
    thread_id: &str,
) -> anyhow::Result<Option<ThreadUsageLatestRow>> {
    let sql = &format!(
        "SELECT {THREAD_USAGE_LATEST_COLUMNS} FROM thread_usage_latest WHERE thread_id = ?"
    );
    let mut stmt = db
        .connection()
        .prepare(sql)
        .context("prepare get_thread_usage_latest")?;
    stmt.query_row([thread_id], ThreadUsageLatestRow::from_row)
        .optional()
        .context("query get_thread_usage_latest")
}

pub fn sum_thread_usage_latest_by_chain(
    db: &ProjectionDb,
    chain_root_id: &str,
) -> anyhow::Result<ThreadUsageTotals> {
    let sql = format!(
        "{EFFECTIVE_USAGE_CTE}
         SELECT
            COUNT(DISTINCT u.thread_id) AS thread_count,
            COALESCE(SUM(u.completed_turns), 0) AS completed_turns,
            COALESCE(SUM(u.input_tokens), 0) AS input_tokens,
            COALESCE(SUM(u.output_tokens), 0) AS output_tokens,
            COALESCE(SUM(u.spend_usd), 0.0) AS spend_usd
         FROM effective_usage u
         WHERE u.chain_root_id = ?"
    );
    db.connection()
        .query_row(&sql, [chain_root_id], |row| {
            Ok(ThreadUsageTotals {
                thread_count: row.get("thread_count")?,
                completed_turns: row.get("completed_turns")?,
                input_tokens: row.get("input_tokens")?,
                output_tokens: row.get("output_tokens")?,
                spend_usd: row.get("spend_usd")?,
            })
        })
        .context("query sum_thread_usage_latest_by_chain")
}

pub fn get_thread_usage_subject(
    db: &ProjectionDb,
    chain_root_id: &str,
) -> anyhow::Result<Option<ThreadUsageSubjectRow>> {
    let mut stmt = db
        .connection()
        .prepare(
            "SELECT chain_root_id, namespace, subject, asserted_by, created_at \
             FROM thread_usage_subjects WHERE chain_root_id = ?",
        )
        .context("prepare get_thread_usage_subject")?;
    stmt.query_row([chain_root_id], ThreadUsageSubjectRow::from_row)
        .optional()
        .context("query get_thread_usage_subject")
}

pub fn summarize_usage_by_subject(
    db: &ProjectionDb,
    filter: UsageSummaryFilter<'_>,
) -> anyhow::Result<Vec<UsageSummaryRow>> {
    let sql = format!(
        "{EFFECTIVE_USAGE_CTE}
         SELECT
            s.namespace AS namespace,
            s.subject AS subject,
            u.provider_id AS provider_id,
            u.model AS model,
            u.profile AS profile,
            COUNT(DISTINCT s.chain_root_id) AS chain_count,
            COUNT(DISTINCT u.thread_id) AS thread_count,
            COALESCE(SUM(u.completed_turns), 0) AS completed_turns,
            COALESCE(SUM(u.input_tokens), 0) AS input_tokens,
            COALESCE(SUM(u.output_tokens), 0) AS output_tokens,
            COALESCE(SUM(u.spend_usd), 0.0) AS spend_usd
         FROM thread_usage_subjects s
         JOIN effective_usage u ON u.chain_root_id = s.chain_root_id
         WHERE (?1 IS NULL OR s.namespace = ?1)
           AND (?2 IS NULL OR s.subject = ?2)
           AND (?3 IS NULL OR s.asserted_by = ?3)
           AND (?4 IS NULL OR u.settled_at >= ?4)
           AND (?5 IS NULL OR u.settled_at < ?5)
         GROUP BY s.namespace, s.subject, u.provider_id, u.model, u.profile
         ORDER BY s.namespace, s.subject, u.provider_id, u.model, u.profile"
    );
    let mut stmt = db
        .connection()
        .prepare(&sql)
        .context("prepare summarize_usage_by_subject")?;
    let rows = stmt
        .query_map(
            rusqlite::params![
                filter.namespace,
                filter.subject,
                filter.asserted_by,
                filter.settled_at_gte,
                filter.settled_at_lt,
            ],
            UsageSummaryRow::from_row,
        )
        .context("query summarize_usage_by_subject")?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

// ============= Tests =============

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::thread_event::NewEvent;
    use crate::objects::thread_snapshot::{ThreadSnapshotBuilder, ThreadStatus};
    use crate::projection::{project_event, project_thread_edge, project_thread_snapshot};
    use serde_json::json;

    fn test_db() -> ProjectionDb {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("test.db");
        ProjectionDb::open(&path).unwrap()
    }

    fn insert_thread(
        db: &ProjectionDb,
        thread_id: &str,
        chain_root_id: &str,
        status: ThreadStatus,
    ) {
        insert_thread_with_upstream(db, thread_id, chain_root_id, status, None);
    }

    fn insert_thread_with_upstream(
        db: &ProjectionDb,
        thread_id: &str,
        chain_root_id: &str,
        status: ThreadStatus,
        upstream_thread_id: Option<&str>,
    ) {
        let snapshot = ThreadSnapshotBuilder::new(
            thread_id,
            chain_root_id,
            "directive",
            "system/test",
            "directive-runtime",
        )
        .status(status)
        .upstream_thread_id(upstream_thread_id.map(str::to_string))
        .build();
        project_thread_snapshot(db, &snapshot, chain_root_id).unwrap();
    }

    fn project_continuation_event(
        db: &ProjectionDb,
        chain_root_id: &str,
        source_thread_id: &str,
        successor_thread_id: &str,
        chain_seq: u64,
    ) {
        let event = NewEvent::new(chain_root_id, source_thread_id, "thread_continued")
            .chain_seq(chain_seq)
            .thread_seq(chain_seq)
            .payload(json!({
                "successor_thread_id": successor_thread_id,
                "reason": "test",
            }))
            .build_with_ts(format!("2026-06-01T00:{chain_seq:02}:00Z"));
        project_event(db, &event).unwrap();
    }

    fn project_usage_event(
        db: &ProjectionDb,
        chain_root_id: &str,
        thread_id: &str,
        chain_seq: u64,
        input_tokens: u64,
        output_tokens: u64,
    ) {
        let event = NewEvent::new(chain_root_id, thread_id, "thread_usage")
            .chain_seq(chain_seq)
            .thread_seq(chain_seq)
            .payload(json!({
                "completed_turns": chain_seq,
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "spend_usd": (input_tokens + output_tokens) as f64 / 1000.0,
                "spawns_used": 0,
                "started_at": "2026-06-01T00:00:00Z",
                "settled_at": format!("2026-06-01T00:0{chain_seq}:00Z"),
                "last_settled_turn_seq": chain_seq,
                "elapsed_ms": chain_seq * 100,
            }))
            .build_with_ts(format!("2026-06-01T00:0{chain_seq}:00Z"));
        project_event(db, &event).unwrap();
    }

    #[test]
    fn get_thread_returns_inserted_thread() {
        let db = test_db();
        insert_thread(&db, "T-1", "chain-A", ThreadStatus::Created);

        let row = get_thread(&db, "T-1").unwrap().unwrap();
        assert_eq!(row.thread_id, "T-1");
        assert_eq!(row.chain_root_id, "chain-A");
        assert_eq!(row.status, "created");
        assert_eq!(row.kind, "directive");
        assert_eq!(row.launch_mode, "inline");
    }

    #[test]
    fn get_thread_returns_none_for_missing() {
        let db = test_db();
        assert!(get_thread(&db, "nope").unwrap().is_none());
    }

    #[test]
    fn list_threads_by_chain_filters_correctly() {
        let db = test_db();
        insert_thread(&db, "T-1", "chain-A", ThreadStatus::Created);
        insert_thread(&db, "T-2", "chain-A", ThreadStatus::Running);
        insert_thread(&db, "T-3", "chain-B", ThreadStatus::Created);

        let rows = list_threads_by_chain(&db, "chain-A").unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.chain_root_id == "chain-A"));

        let rows_b = list_threads_by_chain(&db, "chain-B").unwrap();
        assert_eq!(rows_b.len(), 1);
        assert_eq!(rows_b[0].thread_id, "T-3");
    }

    #[test]
    fn list_threads_by_chain_empty() {
        let db = test_db();
        let rows = list_threads_by_chain(&db, "chain-X").unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn list_threads_by_status_filters() {
        let db = test_db();
        insert_thread(&db, "T-1", "chain-A", ThreadStatus::Created);
        insert_thread(&db, "T-2", "chain-A", ThreadStatus::Running);
        insert_thread(&db, "T-3", "chain-A", ThreadStatus::Completed);

        let rows = list_threads_by_status(&db, &["created", "running"]).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn list_threads_limits() {
        let db = test_db();
        insert_thread(&db, "T-1", "chain-A", ThreadStatus::Created);
        insert_thread(&db, "T-2", "chain-A", ThreadStatus::Created);

        let rows = list_threads(&db, 1).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn active_thread_count_excludes_terminal() {
        let db = test_db();
        insert_thread(&db, "T-1", "chain-A", ThreadStatus::Created);
        insert_thread(&db, "T-2", "chain-A", ThreadStatus::Running);
        insert_thread(&db, "T-3", "chain-A", ThreadStatus::Completed);
        insert_thread(&db, "T-4", "chain-A", ThreadStatus::Failed);

        assert_eq!(active_thread_count(&db).unwrap(), 2);
    }

    #[test]
    fn get_thread_result_returns_none_when_missing() {
        let db = test_db();
        assert!(get_thread_result(&db, "T-x").unwrap().is_none());
    }

    #[test]
    fn list_thread_children_via_edges() {
        let db = test_db();
        insert_thread(&db, "T-parent", "chain-A", ThreadStatus::Running);
        insert_thread(&db, "T-child1", "chain-A", ThreadStatus::Created);
        insert_thread(&db, "T-child2", "chain-A", ThreadStatus::Created);

        project_thread_edge(
            &db,
            "chain-A",
            "T-parent",
            "T-child1",
            Some(1),
            Some("spawn"),
        )
        .unwrap();
        project_thread_edge(&db, "chain-A", "T-parent", "T-child2", Some(2), None).unwrap();

        let children = list_thread_children(&db, "T-parent").unwrap();
        assert_eq!(children.len(), 2);
        let ids: Vec<&str> = children.iter().map(|c| c.thread_id.as_str()).collect();
        assert!(ids.contains(&"T-child1"));
        assert!(ids.contains(&"T-child2"));
    }

    #[test]
    fn list_thread_edges_by_chain() {
        let db = test_db();
        project_thread_edge(&db, "chain-A", "T-p", "T-c1", Some(1), Some("reason")).unwrap();
        project_thread_edge(&db, "chain-A", "T-p", "T-c2", None, None).unwrap();

        let edges = list_thread_edges(&db, "chain-A").unwrap();
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].parent_thread_id, "T-p");
        assert_eq!(edges[0].child_thread_id, "T-c1");
        assert_eq!(edges[0].spawn_seq, Some(1));
        assert_eq!(edges[0].spawn_reason, Some("reason".to_string()));
        assert_eq!(edges[1].spawn_seq, None);
    }

    #[test]
    fn replay_events_filters_by_thread_and_seq() {
        let db = test_db();
        let conn = db.connection();
        conn.execute(
            "INSERT INTO events (chain_root_id, chain_seq, thread_id, thread_seq, event_type, durability, ts, payload) \
             VALUES ('chain-A', 1, 'T-1', 0, 'start', 'durable', '2026-01-01T00:00:00Z', X'00')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO events (chain_root_id, chain_seq, thread_id, thread_seq, event_type, durability, ts, payload) \
             VALUES ('chain-A', 2, 'T-1', 1, 'step', 'durable', '2026-01-01T00:01:00Z', X'01')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO events (chain_root_id, chain_seq, thread_id, thread_seq, event_type, durability, ts, payload) \
             VALUES ('chain-A', 3, 'T-2', 0, 'start', 'durable', '2026-01-01T00:02:00Z', X'02')",
            [],
        ).unwrap();

        let all = replay_events(&db, "chain-A", None, None, 10).unwrap();
        assert_eq!(all.len(), 3);

        let filtered = replay_events(&db, "chain-A", Some("T-1"), None, 10).unwrap();
        assert_eq!(filtered.len(), 2);

        let after = replay_events(&db, "chain-A", Some("T-1"), Some(1), 10).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].chain_seq, 2);
    }

    #[test]
    fn chain_head_thread_is_owner_of_highest_chain_seq_event() {
        let db = test_db();
        let conn = db.connection();
        // No events yet -> no head.
        assert_eq!(chain_head_thread(&db, "chain-A").unwrap(), None);

        conn.execute(
            "INSERT INTO events (chain_root_id, chain_seq, thread_id, thread_seq, event_type, durability, ts, payload) \
             VALUES ('chain-A', 1, 'T-1', 0, 'start', 'durable', '2026-01-01T00:00:00Z', X'00')",
            [],
        ).unwrap();
        assert_eq!(
            chain_head_thread(&db, "chain-A").unwrap(),
            Some("T-1".to_string())
        );

        // Chain advances to a successor: the head follows the latest event,
        // independent of insertion timestamps.
        conn.execute(
            "INSERT INTO events (chain_root_id, chain_seq, thread_id, thread_seq, event_type, durability, ts, payload) \
             VALUES ('chain-A', 2, 'T-2', 0, 'start', 'durable', '2026-01-01T00:00:00Z', X'01')",
            [],
        ).unwrap();
        assert_eq!(
            chain_head_thread(&db, "chain-A").unwrap(),
            Some("T-2".to_string())
        );

        // A different chain is unaffected.
        assert_eq!(chain_head_thread(&db, "chain-B").unwrap(), None);
    }

    #[test]
    fn latest_thread_events_returns_last_n_in_chronological_order() {
        let db = test_db();
        let conn = db.connection();
        for seq in 1..=4 {
            conn.execute(
                "INSERT INTO events (chain_root_id, chain_seq, thread_id, thread_seq, event_type, durability, ts, payload) \
                 VALUES ('chain-A', ?, 'T-1', ?, 'step', 'durable', '2026-01-01T00:00:00Z', X'00')",
                (seq, seq),
            )
            .unwrap();
        }

        let latest = latest_thread_events(&db, "T-1", 2).unwrap();
        assert_eq!(latest.len(), 2);
        assert_eq!(latest[0].chain_seq, 3);
        assert_eq!(latest[1].chain_seq, 4);
    }

    #[test]
    fn list_thread_artifacts_empty() {
        let db = test_db();
        let arts = list_thread_artifacts(&db, "T-1").unwrap();
        assert!(arts.is_empty());
    }

    #[test]
    fn thread_usage_latest_keeps_newest_cumulative_event() {
        let db = test_db();
        insert_thread(&db, "T-1", "chain-A", ThreadStatus::Completed);

        project_usage_event(&db, "chain-A", "T-1", 1, 100, 10);
        project_usage_event(&db, "chain-A", "T-1", 2, 175, 25);
        project_usage_event(&db, "chain-A", "T-1", 1, 100, 10);

        let latest = get_thread_usage_latest(&db, "T-1").unwrap().unwrap();
        assert_eq!(latest.chain_seq, 2);
        assert_eq!(latest.input_tokens, 175);
        assert_eq!(latest.output_tokens, 25);
        assert_eq!(latest.completed_turns, 2);

        let totals = sum_thread_usage_latest_by_chain(&db, "chain-A").unwrap();
        assert_eq!(totals.thread_count, 1);
        assert_eq!(totals.input_tokens, 175);
        assert_eq!(totals.output_tokens, 25);
    }

    #[test]
    fn thread_usage_latest_projects_provider_model_metadata() {
        let db = test_db();
        insert_thread(&db, "T-1", "chain-A", ThreadStatus::Completed);

        let event = NewEvent::new("chain-A", "T-1", "thread_usage")
            .chain_seq(1)
            .thread_seq(1)
            .payload(json!({
                "completed_turns": 1,
                "input_tokens": 100,
                "output_tokens": 10,
                "spend_usd": 0.11,
                "spawns_used": 0,
                "started_at": "2026-06-01T00:00:00Z",
                "settled_at": "2026-06-01T00:01:00Z",
                "last_settled_turn_seq": 1,
                "elapsed_ms": 100,
                "provider_id": "openrouter",
                "model": "anthropic/claude-sonnet-4.5",
                "profile": "default",
            }))
            .build_with_ts("2026-06-01T00:01:00Z".to_string());
        project_event(&db, &event).unwrap();

        let latest = get_thread_usage_latest(&db, "T-1").unwrap().unwrap();
        assert_eq!(latest.provider_id.as_deref(), Some("openrouter"));
        assert_eq!(latest.model.as_deref(), Some("anthropic/claude-sonnet-4.5"));
        assert_eq!(latest.profile.as_deref(), Some("default"));
    }

    #[test]
    fn usage_totals_exclude_continued_source_threads() {
        let db = test_db();
        insert_thread(&db, "T-source", "chain-A", ThreadStatus::Running);
        project_usage_event(&db, "chain-A", "T-source", 1, 100, 10);
        insert_thread(&db, "T-source", "chain-A", ThreadStatus::Continued);

        insert_thread_with_upstream(
            &db,
            "T-successor",
            "chain-A",
            ThreadStatus::Completed,
            Some("T-source"),
        );
        project_continuation_event(&db, "chain-A", "T-source", "T-successor", 2);
        project_usage_event(&db, "chain-A", "T-successor", 2, 150, 15);

        let totals = sum_thread_usage_latest_by_chain(&db, "chain-A").unwrap();
        assert_eq!(totals.thread_count, 1);
        assert_eq!(totals.input_tokens, 150);
        assert_eq!(totals.output_tokens, 15);
    }

    #[test]
    fn usage_totals_exclude_completed_operator_source_threads() {
        // Operator follow-up: the predecessor stays `completed` (its terminal
        // snapshot is preserved), but its successor's usage is cumulative. The
        // predecessor must still be superseded — not double-counted — even
        // though its status is not `continued`.
        let db = test_db();
        insert_thread(&db, "T-source", "chain-A", ThreadStatus::Running);
        project_usage_event(&db, "chain-A", "T-source", 1, 100, 10);
        insert_thread(&db, "T-source", "chain-A", ThreadStatus::Completed);

        insert_thread_with_upstream(
            &db,
            "T-successor",
            "chain-A",
            ThreadStatus::Completed,
            Some("T-source"),
        );
        project_continuation_event(&db, "chain-A", "T-source", "T-successor", 2);
        project_usage_event(&db, "chain-A", "T-successor", 2, 150, 15);

        let totals = sum_thread_usage_latest_by_chain(&db, "chain-A").unwrap();
        assert_eq!(totals.thread_count, 1, "completed predecessor must be superseded");
        assert_eq!(totals.input_tokens, 150);
        assert_eq!(totals.output_tokens, 15);
    }

    #[test]
    fn usage_totals_keep_continued_source_until_successor_reports_usage() {
        let db = test_db();
        insert_thread(&db, "T-source", "chain-A", ThreadStatus::Running);
        project_usage_event(&db, "chain-A", "T-source", 1, 100, 10);
        insert_thread(&db, "T-source", "chain-A", ThreadStatus::Continued);
        insert_thread_with_upstream(
            &db,
            "T-successor",
            "chain-A",
            ThreadStatus::Running,
            Some("T-source"),
        );
        project_continuation_event(&db, "chain-A", "T-source", "T-successor", 2);

        let totals = sum_thread_usage_latest_by_chain(&db, "chain-A").unwrap();
        assert_eq!(totals.thread_count, 1);
        assert_eq!(totals.input_tokens, 100);
        assert_eq!(totals.output_tokens, 10);
    }

    #[test]
    fn usage_totals_resolve_multihop_continuation_to_newest_usage_bearing_thread() {
        let db = test_db();
        insert_thread(&db, "T-a", "chain-A", ThreadStatus::Running);
        project_usage_event(&db, "chain-A", "T-a", 1, 100, 10);
        insert_thread(&db, "T-a", "chain-A", ThreadStatus::Continued);
        insert_thread_with_upstream(&db, "T-b", "chain-A", ThreadStatus::Running, Some("T-a"));
        project_continuation_event(&db, "chain-A", "T-a", "T-b", 2);
        project_usage_event(&db, "chain-A", "T-b", 3, 150, 15);
        insert_thread_with_upstream(&db, "T-b", "chain-A", ThreadStatus::Continued, Some("T-a"));
        insert_thread_with_upstream(&db, "T-c", "chain-A", ThreadStatus::Running, Some("T-b"));
        project_continuation_event(&db, "chain-A", "T-b", "T-c", 4);

        let totals = sum_thread_usage_latest_by_chain(&db, "chain-A").unwrap();
        assert_eq!(totals.thread_count, 1);
        assert_eq!(totals.input_tokens, 150);
        assert_eq!(totals.output_tokens, 15);

        project_usage_event(&db, "chain-A", "T-c", 5, 200, 20);
        let totals = sum_thread_usage_latest_by_chain(&db, "chain-A").unwrap();
        assert_eq!(totals.thread_count, 1);
        assert_eq!(totals.input_tokens, 200);
        assert_eq!(totals.output_tokens, 20);
    }

    #[test]
    fn usage_totals_do_not_treat_normal_child_as_continuation_successor() {
        let db = test_db();
        insert_thread(&db, "T-parent", "chain-A", ThreadStatus::Continued);
        project_usage_event(&db, "chain-A", "T-parent", 1, 100, 10);
        insert_thread_with_upstream(
            &db,
            "T-child",
            "chain-A",
            ThreadStatus::Completed,
            Some("T-parent"),
        );
        project_usage_event(&db, "chain-A", "T-child", 2, 50, 5);

        let totals = sum_thread_usage_latest_by_chain(&db, "chain-A").unwrap();
        assert_eq!(totals.thread_count, 2);
        assert_eq!(totals.input_tokens, 150);
        assert_eq!(totals.output_tokens, 15);
    }

    #[test]
    fn usage_totals_ignore_invalid_continuation_successor_relationship() {
        let db = test_db();
        insert_thread(&db, "T-source", "chain-A", ThreadStatus::Continued);
        project_usage_event(&db, "chain-A", "T-source", 1, 100, 10);
        insert_thread_with_upstream(
            &db,
            "T-not-successor",
            "chain-A",
            ThreadStatus::Completed,
            Some("T-other"),
        );
        project_continuation_event(&db, "chain-A", "T-source", "T-not-successor", 2);
        project_usage_event(&db, "chain-A", "T-not-successor", 3, 50, 5);

        let totals = sum_thread_usage_latest_by_chain(&db, "chain-A").unwrap();
        assert_eq!(totals.thread_count, 2);
        assert_eq!(totals.input_tokens, 150);
        assert_eq!(totals.output_tokens, 15);
    }

    #[test]
    fn thread_created_projects_usage_subject() {
        let db = test_db();
        let event = NewEvent::new("T-root", "T-root", "thread_created")
            .chain_seq(1)
            .thread_seq(1)
            .payload(json!({
                "kind": "directive",
                "item_ref": "directive:apps/tv-tracker/ai_chat",
                "executor_ref": "runtime:directive-runtime",
                "launch_mode": "inline",
                "usage_subject": {
                    "namespace": "tv-tracker",
                    "subject": "csm01"
                },
                "usage_subject_asserted_by": "fp:backend"
            }))
            .build_with_ts("2026-06-01T00:00:00Z".to_string());
        project_event(&db, &event).unwrap();

        let row = get_thread_usage_subject(&db, "T-root").unwrap().unwrap();
        assert_eq!(row.namespace, "tv-tracker");
        assert_eq!(row.subject, "csm01");
        assert_eq!(row.asserted_by.as_deref(), Some("fp:backend"));
        assert_eq!(row.created_at, "2026-06-01T00:00:00Z");
    }

    #[test]
    fn usage_summary_groups_by_subject_and_asserted_by() {
        let db = test_db();

        insert_thread(&db, "T-a", "T-a", ThreadStatus::Completed);
        let event = NewEvent::new("T-a", "T-a", "thread_created")
            .chain_seq(1)
            .thread_seq(1)
            .payload(json!({
                "kind": "directive",
                "item_ref": "directive:apps/tv-tracker/ai_chat",
                "executor_ref": "runtime:directive-runtime",
                "launch_mode": "inline",
                "usage_subject": { "namespace": "tv-tracker", "subject": "user-a" },
                "usage_subject_asserted_by": "fp:backend"
            }))
            .build_with_ts("2026-06-01T00:00:00Z".to_string());
        project_event(&db, &event).unwrap();
        project_usage_event(&db, "T-a", "T-a", 2, 100, 10);

        insert_thread(&db, "T-b", "T-b", ThreadStatus::Completed);
        let event = NewEvent::new("T-b", "T-b", "thread_created")
            .chain_seq(1)
            .thread_seq(1)
            .payload(json!({
                "kind": "directive",
                "item_ref": "directive:apps/tv-tracker/ai_chat",
                "executor_ref": "runtime:directive-runtime",
                "launch_mode": "inline",
                "usage_subject": { "namespace": "tv-tracker", "subject": "user-b" },
                "usage_subject_asserted_by": "fp:other"
            }))
            .build_with_ts("2026-06-01T00:00:00Z".to_string());
        project_event(&db, &event).unwrap();
        project_usage_event(&db, "T-b", "T-b", 2, 300, 30);

        let rows = summarize_usage_by_subject(
            &db,
            UsageSummaryFilter {
                namespace: Some("tv-tracker"),
                asserted_by: Some("fp:backend"),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].namespace, "tv-tracker");
        assert_eq!(rows[0].subject, "user-a");
        assert_eq!(rows[0].chain_count, 1);
        assert_eq!(rows[0].thread_count, 1);
        assert_eq!(rows[0].input_tokens, 100);
        assert_eq!(rows[0].output_tokens, 10);
    }

    #[test]
    fn usage_summary_uses_continuation_effective_usage() {
        let db = test_db();
        insert_thread(&db, "T-root", "T-root", ThreadStatus::Running);
        let event = NewEvent::new("T-root", "T-root", "thread_created")
            .chain_seq(1)
            .thread_seq(1)
            .payload(json!({
                "kind": "directive",
                "item_ref": "directive:apps/tv-tracker/ai_chat",
                "executor_ref": "runtime:directive-runtime",
                "launch_mode": "inline",
                "usage_subject": { "namespace": "tv-tracker", "subject": "user-a" },
                "usage_subject_asserted_by": "fp:backend"
            }))
            .build_with_ts("2026-06-01T00:00:00Z".to_string());
        project_event(&db, &event).unwrap();
        project_usage_event(&db, "T-root", "T-root", 2, 100, 10);
        insert_thread(&db, "T-root", "T-root", ThreadStatus::Continued);
        insert_thread_with_upstream(
            &db,
            "T-successor",
            "T-root",
            ThreadStatus::Completed,
            Some("T-root"),
        );
        project_continuation_event(&db, "T-root", "T-root", "T-successor", 3);
        project_usage_event(&db, "T-root", "T-successor", 4, 150, 15);

        let rows = summarize_usage_by_subject(
            &db,
            UsageSummaryFilter {
                namespace: Some("tv-tracker"),
                asserted_by: Some("fp:backend"),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].thread_count, 1);
        assert_eq!(rows[0].input_tokens, 150);
        assert_eq!(rows[0].output_tokens, 15);
    }
}
