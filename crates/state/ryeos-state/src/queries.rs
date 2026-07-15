//! Query layer for the ryeos-state projection database.
//!
//! Provides typed read accessors for all projection tables plus
//! a small write helper for thread facets.

use anyhow::Context;
use rusqlite::OptionalExtension;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::objects::CapturedThreadHistoryPolicy;
use crate::projection::ProjectionDb;

/// Stay below SQLite's conservative host-parameter ceiling regardless of the
/// connection's compile-time `MAX_VARIABLE_NUMBER`.
const THREAD_ID_QUERY_BATCH: usize = 500;

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
    pub project_root: Option<String>,
    pub base_project_snapshot_hash: Option<String>,
    pub result_project_snapshot_hash: Option<String>,
    pub captured_history_policy: Option<CapturedThreadHistoryPolicy>,
    pub created_at: String,
    pub updated_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

impl ThreadRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let captured_policy_json: Option<String> = row.get("captured_history_policy_json")?;
        let captured_history_policy = captured_policy_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
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
            project_root: row.get("project_root")?,
            base_project_snapshot_hash: row.get("base_project_snapshot_hash")?,
            result_project_snapshot_hash: row.get("result_project_snapshot_hash")?,
            captured_history_policy,
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
    pub event_hash: String,
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
            event_hash: row.get("event_hash")?,
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

    fn replay_serialized_size_upper_bound(&self) -> anyhow::Result<usize> {
        // Covers JSON keys, punctuation, numeric fields, and the outer record
        // framing. Dynamic strings are measured after JSON escaping and the
        // payload is already stored as encoded JSON.
        let mut bytes = self
            .payload
            .len()
            .checked_add(512)
            .context("replay event serialized byte count overflow")?;
        for value in [
            &self.event_hash,
            &self.chain_root_id,
            &self.thread_id,
            &self.event_type,
            &self.durability,
            &self.ts,
        ] {
            bytes = bytes
                .checked_add(serde_json::to_vec(value)?.len())
                .context("replay event serialized byte count overflow")?;
        }
        for value in [
            self.prev_chain_event_hash.as_ref(),
            self.prev_thread_event_hash.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            bytes = bytes
                .checked_add(serde_json::to_vec(value)?.len())
                .context("replay event serialized byte count overflow")?;
        }
        Ok(bytes)
    }
}

#[derive(Debug, Clone)]
pub struct ReplayEventRowsPage {
    pub rows: Vec<EventRow>,
    pub has_more: bool,
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

/// One row in a bounded execution-tree closure. `tree_parent_thread_id` and
/// `relation` describe the structural edge selected by the durable projection;
/// `depth` is returned for diagnostics only (clients derive presentation from
/// ids/parents rather than trusting server-authored indentation).
#[derive(Debug, Clone)]
pub struct ExecutionTreeRow {
    pub thread: ThreadRow,
    pub tree_parent_thread_id: Option<String>,
    pub relation: String,
    pub depth: usize,
    pub has_children: bool,
}

impl ExecutionTreeRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            thread: ThreadRow::from_row(row)?,
            tree_parent_thread_id: row.get("tree_parent_thread_id")?,
            relation: row.get("tree_relation")?,
            depth: row.get::<_, i64>("tree_depth")?.max(0) as usize,
            has_children: row.get("tree_has_children")?,
        })
    }
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
    current_site_id, origin_site_id, upstream_thread_id, requested_by, project_root,
    base_project_snapshot_hash, result_project_snapshot_hash,
    captured_history_policy_json, created_at, updated_at, started_at, finished_at
"#;

pub fn get_thread(db: &ProjectionDb, thread_id: &str) -> anyhow::Result<Option<ThreadRow>> {
    let sql = &format!("SELECT {THREAD_COLUMNS} FROM threads WHERE thread_id = ?");
    let mut stmt = db.connection().prepare(sql).context("prepare get_thread")?;
    stmt.query_row([thread_id], ThreadRow::from_row)
        .optional()
        .context("query get_thread")
}

/// Fetch selected projected thread rows in bounded batches.
pub fn get_threads_many(
    db: &ProjectionDb,
    thread_ids: &[String],
) -> anyhow::Result<Vec<ThreadRow>> {
    let mut threads = Vec::new();
    for batch in thread_ids.chunks(THREAD_ID_QUERY_BATCH) {
        let placeholders = std::iter::repeat("?")
            .take(batch.len())
            .collect::<Vec<_>>()
            .join(",");
        if placeholders.is_empty() {
            continue;
        }
        let sql =
            format!("SELECT {THREAD_COLUMNS} FROM threads WHERE thread_id IN ({placeholders})");
        let mut stmt = db
            .connection()
            .prepare(&sql)
            .context("prepare get_threads_many")?;
        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(batch.iter()),
                ThreadRow::from_row,
            )
            .context("query get_threads_many")?;
        threads.extend(
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .context("read get_threads_many rows")?,
        );
    }
    Ok(threads)
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
/// Row ordering for a thread listing. `Default` is the oldest-first order
/// (public `threads.list`, CLI); `Newest` is newest-first — the "what just
/// ran" order, which matters because the limit truncates (oldest-first +
/// limit returns the OLDEST rows); `Watch` is the operator watch-console
/// order: active threads (non-terminal status) first, then newest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThreadSort {
    #[default]
    Default,
    Newest,
    Watch,
}

pub fn list_threads_filtered(
    db: &ProjectionDb,
    limit: usize,
    filter_principal: Option<&str>,
) -> anyhow::Result<Vec<ThreadRow>> {
    list_threads_sorted(db, limit, filter_principal, ThreadSort::Default)
}

/// Optional filters for a thread listing. `principal` is the authorization
/// scope (public listings restrict to the caller's own threads, matched
/// EXACTLY); `status`/`kind`/`requested_by` are the operator dashboard's
/// optional facets, matched by substring so a type-to-filter box narrows.
/// Every `None` field is simply omitted from the `WHERE`, so an unset filter
/// widens rather than empties the list. Set fields are ANDed.
#[derive(Debug, Clone, Default)]
pub struct ThreadListFilter {
    pub principal: Option<String>,
    pub status: Option<String>,
    pub kind: Option<String>,
    pub requested_by: Option<String>,
    /// Cohort/fleet membership: keep only threads carrying facet `key == value`
    /// (e.g. `("fleet", "<run id>")`). Exact match — a cohort id is not a
    /// substring search.
    pub facet: Option<(String, String)>,
    /// Keep only ACTIVE (non-terminal) threads — the agent's live cognition,
    /// what the one key is running now rather than the settled history.
    pub active_only: bool,
    /// Exclude item refs beginning with any of these prefixes. Exclusions are
    /// applied in SQL before ordering and limit.
    pub exclude_item_prefixes: Vec<String>,
    /// Exact local project scope. Unattributed rows do not match.
    pub project_root: Option<PathBuf>,
}

pub fn list_threads_sorted(
    db: &ProjectionDb,
    limit: usize,
    filter_principal: Option<&str>,
    sort: ThreadSort,
) -> anyhow::Result<Vec<ThreadRow>> {
    list_threads_query(
        db,
        limit,
        &ThreadListFilter {
            principal: filter_principal.map(str::to_string),
            ..Default::default()
        },
        sort,
    )
}

/// The general thread listing: optional [`ThreadListFilter`] + [`ThreadSort`].
/// The `Watch` order sorts active-before-terminal, then newest — for the
/// operator dashboard — without changing the default order the public list /
/// CLI use. Ordering is applied BEFORE `LIMIT`, so a limited watch list still
/// shows the most relevant (active + recent) rows. Each present filter is ANDed
/// (principal exact, dashboard facets substring); absent ones are omitted so
/// the list stays wide.
pub fn list_threads_query(
    db: &ProjectionDb,
    limit: usize,
    filter: &ThreadListFilter,
    sort: ThreadSort,
) -> anyhow::Result<Vec<ThreadRow>> {
    // Terminal statuses inlined from the shared constant (stable substrate
    // vocabulary, not user input), so `active` = status NOT terminal.
    let order = match sort {
        ThreadSort::Default => "ORDER BY created_at".to_string(),
        ThreadSort::Newest => "ORDER BY created_at DESC".to_string(),
        ThreadSort::Watch => {
            let terminal_in = TERMINAL_STATUSES
                .iter()
                .map(|s| format!("'{s}'"))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "ORDER BY CASE WHEN status IN ({terminal_in}) THEN 1 ELSE 0 END, created_at DESC"
            )
        }
    };
    // Build the WHERE from the present filters only; each contributes one bound
    // parameter, so there is no injection surface and an absent filter widens.
    // The owner-scope `principal` is EXACT (an authorization boundary must not
    // widen by substring); the dashboard facets are substring (contains) so a
    // type-to-filter box narrows as the operator types.
    let mut conditions: Vec<&str> = Vec::new();
    let mut params: Vec<&dyn rusqlite::types::ToSql> = Vec::new();
    // `thread_facets.value` is a BLOB, so bind the facet value as bytes to match
    // (a TEXT param would compare unequal to the stored BLOB). Precomputed here so
    // the owned Vec outlives the borrowed params slice.
    let facet_value_bytes: Option<Vec<u8>> =
        filter.facet.as_ref().map(|(_, v)| v.as_bytes().to_vec());
    if let Some(principal) = &filter.principal {
        conditions.push("requested_by = ?");
        params.push(principal);
    }
    if let Some(status) = &filter.status {
        conditions.push("status LIKE '%' || ? || '%'");
        params.push(status);
    }
    if let Some(kind) = &filter.kind {
        conditions.push("kind LIKE '%' || ? || '%'");
        params.push(kind);
    }
    if let Some(requested_by) = &filter.requested_by {
        conditions.push("requested_by LIKE '%' || ? || '%'");
        params.push(requested_by);
    }
    if let (Some((key, _)), Some(value_bytes)) = (&filter.facet, &facet_value_bytes) {
        conditions
            .push("thread_id IN (SELECT thread_id FROM thread_facets WHERE key = ? AND value = ?)");
        params.push(key);
        params.push(value_bytes);
    }
    if filter.active_only {
        // Active = not terminal. The six terminal statuses are stable substrate
        // vocabulary (see TERMINAL_STATUSES), inlined as a fixed placeholder
        // list so this stays a borrowed &str condition.
        conditions.push("status NOT IN (?, ?, ?, ?, ?, ?)");
        for status in TERMINAL_STATUSES.iter() {
            params.push(status);
        }
    }
    for prefix in &filter.exclude_item_prefixes {
        if prefix.is_empty() {
            continue;
        }
        // Keep the literal, case-sensitive semantics of Rust starts_with;
        // LIKE would interpret '%' and '_' in an authored prefix as wildcards.
        conditions.push("substr(item_ref, 1, length(?)) != ?");
        params.push(prefix);
        params.push(prefix);
    }
    let project_root = filter
        .project_root
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    if let Some(project_root) = &project_root {
        conditions.push("project_root = ?");
        params.push(project_root);
    }
    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    let sql = format!("SELECT {THREAD_COLUMNS} FROM threads {where_clause} {order} LIMIT ?");
    params.push(&limit);
    let mut stmt = db
        .connection()
        .prepare(&sql)
        .context("prepare list_threads_query")?;
    let rows = stmt
        .query_map(params.as_slice(), ThreadRow::from_row)
        .context("query list_threads_query")?;
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
    let max_content_bytes = crate::objects::MAX_THREAD_EVENT_SERIALIZED_BYTES;
    let lengths = db
        .connection()
        .query_row(
            "SELECT length(result), length(CAST(error AS BLOB)), \
                    COALESCE(length(CAST(thread_id AS BLOB)), 0) \
                  + COALESCE(length(CAST(chain_root_id AS BLOB)), 0) \
                  + COALESCE(length(CAST(status AS BLOB)), 0) \
                  + COALESCE(length(result), 0) \
                  + COALESCE(length(CAST(outcome_code AS BLOB)), 0) \
                  + COALESCE(length(CAST(error AS BLOB)), 0) \
                  + COALESCE(length(CAST(updated_at AS BLOB)), 0) \
             FROM thread_results WHERE thread_id = ?1",
            [thread_id],
            |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .context("query get_thread_result lengths")?;
    let Some((result_bytes, error_bytes, row_bytes)) = lengths else {
        return Ok(None);
    };
    let result_bytes = usize::try_from(result_bytes.unwrap_or(0))
        .context("thread result has an invalid byte length")?;
    let error_bytes = usize::try_from(error_bytes.unwrap_or(0))
        .context("thread error has an invalid byte length")?;
    let row_bytes =
        usize::try_from(row_bytes).context("thread result row has an invalid length")?;
    if result_bytes > max_content_bytes {
        anyhow::bail!(
            "thread {thread_id} result is {result_bytes} bytes; maximum is {max_content_bytes}"
        );
    }
    if error_bytes > max_content_bytes {
        anyhow::bail!(
            "thread {thread_id} error is {error_bytes} bytes; maximum is {max_content_bytes}"
        );
    }
    let total_bytes = result_bytes
        .checked_add(error_bytes)
        .context("thread result content byte count overflow")?;
    if total_bytes > max_content_bytes {
        anyhow::bail!(
            "thread {thread_id} result and error total {total_bytes} bytes; maximum is {max_content_bytes}"
        );
    }
    if row_bytes > max_content_bytes {
        anyhow::bail!(
            "thread {thread_id} result row is {row_bytes} bytes; maximum is {max_content_bytes}"
        );
    }

    let max_content_bytes_sql = i64::try_from(max_content_bytes)
        .context("thread result byte maximum exceeds SQLite i64")?;
    let mut stmt = db
        .connection()
        .prepare(
            "SELECT thread_id, chain_root_id, status, result, outcome_code, error, updated_at \
             FROM thread_results WHERE thread_id = ?1 \
               AND (result IS NULL OR length(result) <= ?2) \
               AND (error IS NULL OR length(CAST(error AS BLOB)) <= ?2) \
               AND COALESCE(length(result), 0) \
                   + COALESCE(length(CAST(error AS BLOB)), 0) <= ?2 \
               AND COALESCE(length(CAST(thread_id AS BLOB)), 0) \
                   + COALESCE(length(CAST(chain_root_id AS BLOB)), 0) \
                   + COALESCE(length(CAST(status AS BLOB)), 0) \
                   + COALESCE(length(result), 0) \
                   + COALESCE(length(CAST(outcome_code AS BLOB)), 0) \
                   + COALESCE(length(CAST(error AS BLOB)), 0) \
                   + COALESCE(length(CAST(updated_at AS BLOB)), 0) <= ?2",
        )
        .context("prepare get_thread_result")?;
    let row = stmt
        .query_row(
            rusqlite::params![thread_id, max_content_bytes_sql],
            ThreadResultRow::from_row,
        )
        .optional()
        .context("query get_thread_result")?;
    row.map(Some).ok_or_else(|| {
        anyhow::anyhow!(
            "thread {thread_id} result changed after its bounded length preflight or exceeds the byte maximum"
        )
    })
}

pub fn list_thread_artifacts_bounded(
    db: &ProjectionDb,
    thread_id: &str,
    limit: usize,
    max_kind_bytes: usize,
    max_metadata_bytes: usize,
    max_total_metadata_bytes: usize,
) -> anyhow::Result<Vec<ArtifactRow>> {
    if limit == 0 {
        anyhow::bail!("thread artifact limit must be positive");
    }
    let mut stmt = db
        .connection()
        .prepare(
            "SELECT chain_root_id, thread_id, \
                    CASE WHEN length(CAST(kind AS BLOB)) <= ?3 THEN kind ELSE NULL END AS kind, \
                    CASE WHEN metadata IS NULL OR length(metadata) <= ?4 THEN metadata ELSE NULL END AS metadata, \
                    created_at, length(CAST(kind AS BLOB)) AS kind_len, \
                    length(metadata) AS metadata_len \
             FROM thread_artifacts WHERE thread_id = ?1 \
             ORDER BY created_at, artifact_id LIMIT ?2",
        )
        .context("prepare bounded thread artifacts")?;
    let sql_limit = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
    let mut rows = stmt
        .query(rusqlite::params![
            thread_id,
            sql_limit,
            i64::try_from(max_kind_bytes).unwrap_or(i64::MAX),
            i64::try_from(max_metadata_bytes).unwrap_or(i64::MAX),
        ])
        .context("query bounded thread artifacts")?;
    let mut artifacts = Vec::with_capacity(limit.min(32));
    let mut total_metadata_bytes = 0usize;
    while let Some(row) = rows.next().context("read bounded thread artifact row")? {
        if artifacts.len() == limit {
            anyhow::bail!("thread {thread_id} exceeds the {limit}-artifact maximum");
        }
        let kind_len = usize::try_from(row.get::<_, i64>("kind_len")?)
            .context("thread artifact kind has an invalid length")?;
        if kind_len == 0 || kind_len > max_kind_bytes {
            anyhow::bail!(
                "thread {thread_id} artifact kind is {kind_len} bytes; maximum is {max_kind_bytes}"
            );
        }
        let metadata_len = row.get::<_, Option<i64>>("metadata_len")?.unwrap_or(0);
        let metadata_len = usize::try_from(metadata_len)
            .context("thread artifact metadata has an invalid negative/oversized length")?;
        if metadata_len > max_metadata_bytes {
            anyhow::bail!(
                "thread {thread_id} artifact metadata is {metadata_len} bytes; maximum is {max_metadata_bytes}"
            );
        }
        total_metadata_bytes = total_metadata_bytes
            .checked_add(metadata_len)
            .context("thread artifact metadata total overflow")?;
        if total_metadata_bytes > max_total_metadata_bytes {
            anyhow::bail!(
                "thread {thread_id} artifact metadata exceeds the {max_total_metadata_bytes}-byte total"
            );
        }
        // Read the BLOB only after its SQLite length has passed the bound.
        artifacts.push(ArtifactRow::from_row(row)?);
    }
    Ok(artifacts)
}

pub fn thread_artifact_stats(
    db: &ProjectionDb,
    thread_id: &str,
) -> anyhow::Result<(usize, usize, usize)> {
    let (count, kind_bytes, metadata_bytes): (i64, i64, i64) = db.connection().query_row(
        "SELECT COUNT(*), COALESCE(SUM(length(CAST(kind AS BLOB))), 0), \
                COALESCE(SUM(COALESCE(length(metadata), 0)), 0) \
         FROM thread_artifacts WHERE thread_id = ?",
        [thread_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok((
        usize::try_from(count).context("thread artifact count is invalid")?,
        usize::try_from(kind_bytes).context("thread artifact kind byte total is invalid")?,
        usize::try_from(metadata_bytes).context("thread artifact byte total is invalid")?,
    ))
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

pub fn thread_edge_exists(
    db: &ProjectionDb,
    parent_thread_id: &str,
    child_thread_id: &str,
) -> anyhow::Result<bool> {
    db.connection()
        .query_row(
            "SELECT EXISTS (
                SELECT 1 FROM thread_edges
                WHERE parent_thread_id = ?1 AND child_thread_id = ?2
            )",
            rusqlite::params![parent_thread_id, child_thread_id],
            |row| row.get(0),
        )
        .context("query thread_edge_exists")
}

/// Resolve `selected_thread_id` to the oldest reachable execution ancestor,
/// then return a deterministic pre-order closure over both durable edge kinds:
/// continuation (`threads.upstream_thread_id`) and cross-chain spawn
/// (`thread_edges`). The caller supplies hard depth/node bounds; cycles are
/// rejected by the path guard instead of making the query unbounded.
pub fn execution_tree(
    db: &ProjectionDb,
    selected_thread_id: &str,
    max_depth: usize,
    row_limit: usize,
) -> anyhow::Result<Vec<ExecutionTreeRow>> {
    let sql = r#"
        WITH RECURSIVE
        edges(parent_thread_id, child_thread_id, relation, created_at) AS (
            SELECT
                e.parent_thread_id,
                e.child_thread_id,
                CASE
                    WHEN MAX(
                        CASE WHEN child.upstream_thread_id = e.parent_thread_id THEN 1 ELSE 0 END
                    ) = 1 THEN 'continued'
                    WHEN MAX(CASE WHEN e.spawn_reason = 'follow' THEN 1 ELSE 0 END) = 1
                        THEN 'follow'
                    ELSE 'spawned'
                END,
                MIN(COALESCE(NULLIF(e.created_at, ''), child.created_at))
            FROM thread_edges e
            JOIN threads child ON child.thread_id = e.child_thread_id
            GROUP BY e.parent_thread_id, e.child_thread_id
        ),
        ancestors(thread_id, depth, path) AS (
            SELECT ?1, 0, ',' || ?1 || ','
            WHERE EXISTS (SELECT 1 FROM threads WHERE thread_id = ?1)
            UNION ALL
            SELECT e.parent_thread_id,
                   a.depth + 1,
                   a.path || e.parent_thread_id || ','
            FROM ancestors a
            JOIN edges e ON e.child_thread_id = a.thread_id
            WHERE a.depth < ?2
              AND instr(a.path, ',' || e.parent_thread_id || ',') = 0
        ),
        root(thread_id) AS (
            SELECT thread_id
            FROM ancestors
            ORDER BY depth DESC, thread_id
            LIMIT 1
        ),
        walk(thread_id, parent_thread_id, relation, depth, path, sort_path) AS (
            SELECT root.thread_id,
                   NULL,
                   'root',
                   0,
                   ',' || root.thread_id || ',',
                   (SELECT created_at FROM threads WHERE thread_id = root.thread_id)
                       || ':' || root.thread_id
            FROM root
            UNION ALL
            SELECT e.child_thread_id,
                   e.parent_thread_id,
                   e.relation,
                   w.depth + 1,
                   w.path || e.child_thread_id || ',',
                   w.sort_path || '/' || e.created_at || ':' || e.child_thread_id
            FROM walk w
            JOIN edges e ON e.parent_thread_id = w.thread_id
            WHERE w.depth < ?2
              AND instr(w.path, ',' || e.child_thread_id || ',') = 0
        )
        SELECT
            t.thread_id, t.chain_root_id, t.kind, t.status,
            t.item_ref, t.executor_ref, t.launch_mode,
            t.current_site_id, t.origin_site_id, t.upstream_thread_id,
            t.requested_by, t.project_root,
            t.created_at, t.updated_at, t.started_at, t.finished_at,
            walk.parent_thread_id AS tree_parent_thread_id,
            walk.relation AS tree_relation,
            walk.depth AS tree_depth,
            EXISTS (
                SELECT 1 FROM edges child_edge
                WHERE child_edge.parent_thread_id = walk.thread_id
            ) AS tree_has_children
        FROM walk
        JOIN threads t ON t.thread_id = walk.thread_id
        ORDER BY walk.sort_path
        LIMIT ?3
    "#;
    let max_depth = i64::try_from(max_depth).context("execution tree depth is too large")?;
    let row_limit = i64::try_from(row_limit).context("execution tree row limit is too large")?;
    let mut stmt = db
        .connection()
        .prepare(sql)
        .context("prepare execution_tree")?;
    let rows = stmt
        .query_map(
            rusqlite::params![selected_thread_id, max_depth, row_limit],
            ExecutionTreeRow::from_row,
        )
        .context("query execution_tree")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("read execution_tree rows")
}

pub fn replay_events(
    db: &ProjectionDb,
    chain_root_id: &str,
    thread_id: Option<&str>,
    after_seq: Option<i64>,
    limit: usize,
) -> anyhow::Result<Vec<EventRow>> {
    let mut sql = String::from(
        "SELECT event_id, event_hash, chain_root_id, chain_seq, thread_id, thread_seq, \
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
    let sql_limit = i64::try_from(limit).context("replay_events limit exceeds SQLite i64")?;
    params.push(Box::new(sql_limit));

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

/// Replay a bounded page without materializing an unbounded event history.
///
/// The byte budget is a conservative upper bound for serialized response
/// records, including escaped metadata strings and JSON payload bytes. One
/// extra capped row may be inspected to determine whether another page exists.
pub fn replay_events_bounded(
    db: &ProjectionDb,
    chain_root_id: &str,
    thread_id: Option<&str>,
    after_seq: Option<i64>,
    limit: usize,
    max_serialized_bytes: usize,
) -> anyhow::Result<ReplayEventRowsPage> {
    if limit == 0 {
        anyhow::bail!("replay_events_bounded limit must be greater than zero");
    }
    if max_serialized_bytes == 0 {
        anyhow::bail!("replay_events_bounded byte budget must be greater than zero");
    }

    // Returning NULL for an oversized legacy payload lets us reject it before
    // rusqlite allocates the BLOB. Current writers cannot create such a row
    // because the complete ThreadEvent object has the same or smaller ceiling.
    let mut sql = String::from(
        "SELECT event_id, event_hash, chain_root_id, chain_seq, thread_id, thread_seq, \
                event_type, durability, ts, prev_chain_event_hash, \
                prev_thread_event_hash, \
                CASE WHEN length(payload) <= ? THEN payload ELSE NULL END AS payload, \
                length(payload) AS payload_bytes \
         FROM events WHERE chain_root_id = ?",
    );
    let max_event_payload_bytes = crate::objects::MAX_THREAD_EVENT_SERIALIZED_BYTES;
    let sql_max_event_payload_bytes = i64::try_from(max_event_payload_bytes)
        .context("thread event byte limit exceeds SQLite i64")?;
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![
        Box::new(sql_max_event_payload_bytes),
        Box::new(chain_root_id.to_string()),
    ];

    if let Some(tid) = thread_id {
        sql.push_str(" AND thread_id = ?");
        params.push(Box::new(tid.to_string()));
    }
    if let Some(seq) = after_seq {
        sql.push_str(" AND chain_seq > ?");
        params.push(Box::new(seq));
    }

    sql.push_str(" ORDER BY chain_seq LIMIT ?");
    let query_limit = limit
        .checked_add(1)
        .context("replay_events_bounded limit overflow")?;
    let sql_limit =
        i64::try_from(query_limit).context("replay_events_bounded limit exceeds SQLite i64")?;
    params.push(Box::new(sql_limit));

    let mut stmt = db
        .connection()
        .prepare(&sql)
        .context("prepare replay_events_bounded")?;
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut query_rows = stmt
        .query(param_refs.as_slice())
        .context("query replay_events_bounded")?;

    let mut rows = Vec::with_capacity(limit.min(64));
    let mut serialized_bytes = 0usize;
    let mut has_more = false;
    while let Some(row) = query_rows
        .next()
        .context("read replay_events_bounded row")?
    {
        if rows.len() == limit {
            has_more = true;
            break;
        }

        let payload_bytes_i64: i64 = row
            .get("payload_bytes")
            .context("read replay event payload byte count")?;
        let payload_bytes = usize::try_from(payload_bytes_i64)
            .context("replay event payload has invalid byte count")?;
        if payload_bytes > max_event_payload_bytes {
            anyhow::bail!(
                "replay event payload is {} bytes (max {})",
                payload_bytes,
                max_event_payload_bytes
            );
        }

        let event = EventRow::from_row(row).context("decode replay_events_bounded row")?;
        let event_bytes = event.replay_serialized_size_upper_bound()?;
        let next_serialized_bytes = serialized_bytes
            .checked_add(event_bytes)
            .context("replay page serialized byte count overflow")?;
        if next_serialized_bytes > max_serialized_bytes {
            if rows.is_empty() {
                anyhow::bail!(
                    "single replay event requires {} serialized bytes (page budget {})",
                    event_bytes,
                    max_serialized_bytes
                );
            }
            has_more = true;
            break;
        }

        serialized_bytes = next_serialized_bytes;
        rows.push(event);
    }

    Ok(ReplayEventRowsPage { rows, has_more })
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

/// Raw batch counterpart to [`continuation_successor`]. JSON decoding is left
/// to the caller so it can happen after releasing an outer store lock.
pub fn continuation_successor_payloads(
    db: &ProjectionDb,
    thread_ids: &[String],
    max_items: usize,
    max_payload_bytes: usize,
    max_total_bytes: usize,
) -> anyhow::Result<HashMap<String, Vec<u8>>> {
    latest_event_payloads_bounded(
        db,
        thread_ids,
        crate::event_types::THREAD_CONTINUED,
        max_items,
        max_payload_bytes,
        max_total_bytes,
    )
}

/// Return the latest payload of one event type for each selected thread.
/// Aggregate/count-only queries prove the full page fits before a second pass
/// selects any BLOB. The guarded CASE remains a defense against drift or a
/// malformed projection outside the daemon's serialized state-store lock.
fn latest_event_payloads_bounded(
    db: &ProjectionDb,
    thread_ids: &[String],
    event_type: &str,
    max_items: usize,
    max_payload_bytes: usize,
    max_total_bytes: usize,
) -> anyhow::Result<HashMap<String, Vec<u8>>> {
    if thread_ids.is_empty() {
        return Ok(HashMap::new());
    }
    if max_items == 0 {
        anyhow::bail!("latest event payload item budget must be positive");
    }

    let mut total_items = 0usize;
    let mut total_bytes = 0usize;
    for batch in thread_ids.chunks(THREAD_ID_QUERY_BATCH) {
        let placeholders = std::iter::repeat("?")
            .take(batch.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT COUNT(*), COALESCE(SUM(length(e.payload)), 0), \
                    COALESCE(MAX(length(e.payload)), 0) \
             FROM events e INNER JOIN ( \
                 SELECT thread_id, MAX(thread_seq) AS thread_seq FROM events \
                 WHERE event_type = ? AND thread_id IN ({placeholders}) \
                 GROUP BY thread_id \
             ) latest ON latest.thread_id = e.thread_id \
                     AND latest.thread_seq = e.thread_seq \
             WHERE e.event_type = ?"
        );
        let mut params = Vec::with_capacity(batch.len() + 2);
        params.push(rusqlite::types::Value::Text(event_type.to_string()));
        params.extend(batch.iter().cloned().map(rusqlite::types::Value::Text));
        params.push(rusqlite::types::Value::Text(event_type.to_string()));
        let (items, bytes, largest): (i64, i64, i64) =
            db.connection()
                .query_row(&sql, rusqlite::params_from_iter(params.iter()), |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
        let items = usize::try_from(items).context("latest event payload count is invalid")?;
        let bytes = usize::try_from(bytes).context("latest event payload total is invalid")?;
        let largest = usize::try_from(largest).context("latest event payload length is invalid")?;
        if largest > max_payload_bytes {
            anyhow::bail!(
                "latest {event_type} payload is {largest} bytes; maximum is {max_payload_bytes}"
            );
        }
        total_items = total_items
            .checked_add(items)
            .context("latest event payload count overflow")?;
        total_bytes = total_bytes
            .checked_add(bytes)
            .context("latest event payload byte total overflow")?;
        if total_items > max_items {
            anyhow::bail!(
                "latest {event_type} payloads contain {total_items} items; maximum is {max_items}"
            );
        }
        if total_bytes > max_total_bytes {
            anyhow::bail!(
                "latest {event_type} payloads total {total_bytes} bytes; maximum is {max_total_bytes}"
            );
        }
    }

    let mut payloads = HashMap::with_capacity(total_items);
    for batch in thread_ids.chunks(THREAD_ID_QUERY_BATCH) {
        let placeholders = std::iter::repeat("?")
            .take(batch.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT e.thread_id, \
                    CASE WHEN length(e.payload) <= ? THEN e.payload ELSE NULL END, \
                    length(e.payload) \
             FROM events e INNER JOIN ( \
                 SELECT thread_id, MAX(thread_seq) AS thread_seq FROM events \
                 WHERE event_type = ? AND thread_id IN ({placeholders}) \
                 GROUP BY thread_id \
             ) latest ON latest.thread_id = e.thread_id \
                     AND latest.thread_seq = e.thread_seq \
             WHERE e.event_type = ?"
        );
        let mut params = Vec::with_capacity(batch.len() + 3);
        params.push(rusqlite::types::Value::Integer(
            i64::try_from(max_payload_bytes).unwrap_or(i64::MAX),
        ));
        params.push(rusqlite::types::Value::Text(event_type.to_string()));
        params.extend(batch.iter().cloned().map(rusqlite::types::Value::Text));
        params.push(rusqlite::types::Value::Text(event_type.to_string()));
        let mut stmt = db
            .connection()
            .prepare(&sql)
            .context("prepare bounded latest event payloads")?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<Vec<u8>>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .context("query bounded latest event payloads")?;
        for row in rows {
            let (thread_id, payload, payload_len) =
                row.context("read bounded latest event payload row")?;
            let payload_len = usize::try_from(payload_len)
                .context("latest event payload has an invalid length")?;
            if payload_len > max_payload_bytes {
                anyhow::bail!(
                    "latest {event_type} payload is {payload_len} bytes; maximum is {max_payload_bytes}"
                );
            }
            let payload = payload.ok_or_else(|| {
                anyhow::anyhow!("bounded latest {event_type} payload unexpectedly returned NULL")
            })?;
            payloads.insert(thread_id, payload);
        }
    }
    Ok(payloads)
}

/// The `successor_request_fingerprint` recorded in the source's `thread_continued`
/// payload, if any. Used to dedup operator double-submits: a follow-up whose
/// fingerprint matches the recorded one resolves to the existing successor; a
/// different fingerprint is a conflict. `None` when the source is not continued,
/// or when its successor predates fingerprinting (e.g. a machine continuation,
/// which records no operator fingerprint).
pub fn continuation_fingerprint(
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
        .context("query continuation_fingerprint")?;
    let Some(bytes) = payload else {
        return Ok(None);
    };
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).context("parse thread_continued payload")?;
    Ok(value
        .get("successor_request_fingerprint")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()))
}

/// # Follow lineage: the two projected edge kinds
///
/// A graph `follow:` relationship spans two distinct lineage links. Both are
/// recorded in the projection, but from different durable events:
///
/// 1. **Parent → resume-successor (within-chain, projected).** When a followed
///    child terminates, the suspended parent is resumed by minting a successor
///    in the SAME chain (`upstream_thread_id = parent`, same `chain_root_id`).
///    `project_thread_snapshot` derives a `thread_edges` row for it
///    (`spawn_reason = 'spawned'`), and the `thread_continued` payload carries
///    [`ContinuationReasonMarker::GraphFollowResume`] as its `reason` — that
///    marker (read via [`continuation_edge`]) is the discriminator that tells a
///    follow-resume successor from an ordinary segment-cut continuation.
///
/// 2. **Parent → followed child chain root (cross-chain, projected).** The
///    followed child is spawned as a FRESH ROOT — its own `chain_root_id`, no
///    `upstream_thread_id` — so the executor emits `child_thread_spawned` on the
///    parent braid with `spawn_reason = 'follow'`. Event projection derives the
///    cross-chain edge exactly like an ordinary graph dispatch. The runtime DB
///    child link remains a separate operational copy for stop cascading.
///
/// Daemon-owned markers stored as the `reason` on a `thread_continued` edge.
/// Centralizes the wire value so it is not a scattered string literal a runtime
/// could typo or spoof. Machine continuations carry a free-form *log* reason
/// (e.g. `turn_limit`); only this OPERATOR marker is daemon-reserved, and even
/// then the request fingerprint is what actually proves an operator edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationReasonMarker {
    /// An operator follow-up (the explicit-user-turn path).
    OperatorFollowUp,
    /// A parent resuming after a followed child chain terminates. Daemon-written
    /// only — runtime-facing continuation paths scrub all reserved markers — so
    /// it needs no fingerprint to be trusted.
    GraphFollowResume,
}

impl ContinuationReasonMarker {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OperatorFollowUp => "operator_follow_up",
            Self::GraphFollowResume => "graph_follow_resume",
        }
    }

    /// Parse a reserved marker string; `None` for anything caller-supplied
    /// (deliberately not `std::str::FromStr` — absence is the common case).
    pub fn parse_marker(value: &str) -> Option<Self> {
        match value {
            "operator_follow_up" => Some(Self::OperatorFollowUp),
            "graph_follow_resume" => Some(Self::GraphFollowResume),
            _ => None,
        }
    }

    /// Whether `value` is any daemon-reserved marker. Runtime-facing continuation
    /// paths scrub these from caller-supplied reasons so a runtime cannot forge
    /// an operator reset or a depth-exempt follow edge.
    pub fn is_reserved_str(value: &str) -> bool {
        Self::parse_marker(value).is_some()
    }
}

/// `(successor_thread_id, reason, request_fingerprint)` — the tuple
/// [`continuation_edge`] yields.
pub type ContinuationEdge = (String, Option<String>, Option<String>);

/// The continuation EDGE on a source's `thread_continued` payload, if any:
/// `(successor_thread_id, reason, request_fingerprint)`. The fingerprint is
/// present only on OPERATOR follow-ups (`create_or_get_continuation` records it);
/// a machine continuation never does, so it cannot spoof the operator marker by
/// passing `reason == "operator_follow_up"`. Returns `None` when there is no
/// `thread_continued` event or its payload names no successor.
pub fn continuation_edge(
    db: &ProjectionDb,
    thread_id: &str,
) -> anyhow::Result<Option<ContinuationEdge>> {
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
        .context("query continuation_edge")?;
    let Some(bytes) = payload else {
        return Ok(None);
    };
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).context("parse thread_continued payload")?;
    let Some(successor) = value
        .get("successor_thread_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return Ok(None);
    };
    let reason = value
        .get("reason")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let fingerprint = value
        .get("successor_request_fingerprint")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok(Some((successor, reason, fingerprint)))
}

/// Count CONSECUTIVE MACHINE continuations ending at `thread_id`, walking
/// `upstream_thread_id`. Each hop counts ONLY if it is a VERIFIED machine
/// continuation edge:
///
/// - the `upstream` actually has a `thread_continued` edge whose
///   `successor_thread_id` IS the current thread — a bare `upstream_thread_id`
///   from a non-continuation parent/child relationship (e.g. a compose-context
///   child) is not a continuation and stops the walk; and
/// - it is not an OPERATOR follow-up — an operator link is `reason ==
///   "operator_follow_up"` AND carries the operator-only request fingerprint, so
///   a machine continuation cannot reset the cap by spoofing the reason. An
///   operator follow-up RESETS the count (a long operator conversation is never
///   capped).
///
/// Stops at `cap`, an operator link, a non-continuation edge, or the chain root —
/// O(min(actual_depth, cap)) lookups. Bounds the length of an autonomous run.
pub fn consecutive_machine_continuation_depth(
    db: &ProjectionDb,
    thread_id: &str,
    cap: u32,
) -> anyhow::Result<u32> {
    let mut count = 0u32;
    let mut current = thread_id.to_string();
    while count < cap {
        let Some(row) = get_thread(db, &current)? else {
            break;
        };
        let Some(upstream) = row.upstream_thread_id else {
            break; // chain root
        };
        // The edge that created `current` lives on `upstream`'s thread_continued.
        let Some((successor, reason, fingerprint)) = continuation_edge(db, &upstream)? else {
            break; // upstream has no continuation edge — `current`'s upstream link
                   // is not a continuation (e.g. a compose-context child)
        };
        if successor != current {
            break; // upstream continued to a DIFFERENT thread — not this one
        }
        if reason.as_deref() == Some(ContinuationReasonMarker::OperatorFollowUp.as_str())
            && fingerprint.is_some()
        {
            break; // VERIFIED operator link resets the autonomous run
        }
        if reason.as_deref() == Some(ContinuationReasonMarker::GraphFollowResume.as_str()) {
            break; // graph follow-resume edge (daemon-only marker) — structural
                   // progress, not an autonomous segment-cut; resets the run
        }
        count += 1; // verified machine continuation link
        current = upstream;
    }
    Ok(count)
}

pub fn latest_thread_events(
    db: &ProjectionDb,
    thread_id: &str,
    limit: usize,
) -> anyhow::Result<Vec<EventRow>> {
    let mut stmt = db
        .connection()
        .prepare(
            "SELECT event_id, event_hash, chain_root_id, chain_seq, thread_id, thread_seq, \
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

/// Latest durable events across every thread on the node — the node-wide
/// activity feed. Append order (`event_id` is the autoincrement insert
/// order), returned oldest-first after the reverse so feeds read
/// top-to-bottom like a replay. `exclude_types` drops kinds the caller
/// declares as noise (e.g. seat facet writes); the query itself carries
/// no event vocabulary.
pub fn latest_node_events(
    db: &ProjectionDb,
    limit: usize,
    exclude_types: &[String],
) -> anyhow::Result<Vec<EventRow>> {
    let mut sql = String::from(
        "SELECT event_id, event_hash, chain_root_id, chain_seq, thread_id, thread_seq, \
            event_type, durability, ts, prev_chain_event_hash, \
            prev_thread_event_hash, payload \
         FROM events",
    );
    if !exclude_types.is_empty() {
        sql.push_str(" WHERE event_type NOT IN (");
        sql.push_str(&vec!["?"; exclude_types.len()].join(","));
        sql.push(')');
    }
    sql.push_str(" ORDER BY event_id DESC LIMIT ?");
    let mut stmt = db
        .connection()
        .prepare(&sql)
        .context("prepare latest_node_events")?;
    let mut params: Vec<rusqlite::types::Value> = exclude_types
        .iter()
        .map(|t| rusqlite::types::Value::Text(t.clone()))
        .collect();
    params.push(rusqlite::types::Value::Integer(limit as i64));
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), EventRow::from_row)
        .context("query latest_node_events")?;
    let mut events: Vec<EventRow> = rows.filter_map(|r| r.ok()).collect();
    events.reverse();
    Ok(events)
}

/// Per-status thread counts for the node pulse: non-terminal statuses
/// always count (they are "now"), terminal statuses count only when the
/// thread was last touched inside the window (`since_iso`, inclusive —
/// ISO-8601 strings compare lexically).
pub fn thread_status_counts(
    db: &ProjectionDb,
    since_iso: &str,
) -> anyhow::Result<Vec<(String, i64)>> {
    let mut stmt = db
        .connection()
        .prepare(
            "SELECT status, COUNT(*) AS n FROM threads \
             WHERE status IN ('created', 'running') OR updated_at >= ? \
             GROUP BY status ORDER BY status",
        )
        .context("prepare thread_status_counts")?;
    let rows = stmt
        .query_map([since_iso], |row| Ok((row.get(0)?, row.get(1)?)))
        .context("query thread_status_counts")?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub fn get_facets_bounded(
    db: &ProjectionDb,
    thread_id: &str,
    limit: usize,
    max_key_bytes: usize,
    max_value_bytes: usize,
    max_content_bytes: usize,
) -> anyhow::Result<Vec<FacetRow>> {
    if limit == 0 {
        anyhow::bail!("thread facet limit must be positive");
    }
    let mut stmt = db
        .connection()
        .prepare(
            "SELECT thread_id, \
                    CASE WHEN length(CAST(key AS BLOB)) <= ?3 THEN key ELSE NULL END AS key, \
                    CASE WHEN length(value) <= ?4 THEN value ELSE NULL END AS value, \
                    length(CAST(key AS BLOB)) AS key_len, \
                    length(value) AS value_len \
             FROM thread_facets WHERE thread_id = ?1 ORDER BY key LIMIT ?2",
        )
        .context("prepare bounded thread facets")?;
    let sql_limit = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
    let mut rows = stmt
        .query(rusqlite::params![
            thread_id,
            sql_limit,
            i64::try_from(max_key_bytes).unwrap_or(i64::MAX),
            i64::try_from(max_value_bytes).unwrap_or(i64::MAX),
        ])
        .context("query bounded thread facets")?;
    let mut facets = Vec::with_capacity(limit.min(32));
    let mut content_bytes = 0usize;
    while let Some(row) = rows.next().context("read bounded thread facet row")? {
        if facets.len() == limit {
            anyhow::bail!("thread {thread_id} exceeds the {limit}-facet maximum");
        }
        let key_len = usize::try_from(row.get::<_, i64>("key_len")?)
            .context("thread facet key has an invalid length")?;
        let value_len = usize::try_from(row.get::<_, i64>("value_len")?)
            .context("thread facet value has an invalid length")?;
        if key_len > max_key_bytes || value_len > max_value_bytes {
            anyhow::bail!(
                "thread {thread_id} facet exceeds bounds (key={key_len}/{max_key_bytes}, value={value_len}/{max_value_bytes})"
            );
        }
        content_bytes = content_bytes
            .checked_add(key_len)
            .and_then(|bytes| bytes.checked_add(value_len))
            .context("thread facet content total overflow")?;
        if content_bytes > max_content_bytes {
            anyhow::bail!(
                "thread {thread_id} facet content exceeds the {max_content_bytes}-byte total"
            );
        }
        // Read the BLOB only after SQLite's length has passed the bound.
        facets.push(FacetRow::from_row(row)?);
    }
    Ok(facets)
}

pub fn thread_facet_stats(db: &ProjectionDb, thread_id: &str) -> anyhow::Result<(usize, usize)> {
    let (count, content_bytes): (i64, i64) = db.connection().query_row(
        "SELECT COUNT(*), \
                COALESCE(SUM(length(CAST(key AS BLOB)) + length(value)), 0) \
         FROM thread_facets WHERE thread_id = ?",
        [thread_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok((
        usize::try_from(count).context("thread facet count is invalid")?,
        usize::try_from(content_bytes).context("thread facet byte total is invalid")?,
    ))
}

pub fn thread_facet_value_bytes(
    db: &ProjectionDb,
    thread_id: &str,
    key: &str,
) -> anyhow::Result<Option<usize>> {
    let value_bytes = db
        .connection()
        .query_row(
            "SELECT length(value) FROM thread_facets WHERE thread_id = ?1 AND key = ?2",
            rusqlite::params![thread_id, key],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    value_bytes
        .map(|bytes| usize::try_from(bytes).context("thread facet value length is invalid"))
        .transpose()
}

/// Aggregate facet cardinality/content for selected threads without selecting
/// any value BLOBs. Callers use this as a preflight before the guarded fetch.
pub fn thread_facet_stats_many(
    db: &ProjectionDb,
    thread_ids: &[String],
) -> anyhow::Result<Vec<(String, usize, usize)>> {
    if thread_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut stats = Vec::new();
    for batch in thread_ids.chunks(THREAD_ID_QUERY_BATCH) {
        let placeholders = std::iter::repeat("?")
            .take(batch.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT thread_id, COUNT(*), \
                    COALESCE(SUM(length(CAST(key AS BLOB)) + length(value)), 0) \
             FROM thread_facets WHERE thread_id IN ({placeholders}) GROUP BY thread_id"
        );
        let mut stmt = db
            .connection()
            .prepare(&sql)
            .context("prepare many-facet stats")?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(batch.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .context("query many-facet stats")?;
        for row in rows {
            let (thread_id, items, content_bytes) = row?;
            stats.push((
                thread_id,
                usize::try_from(items).context("many-facet count is invalid")?,
                usize::try_from(content_bytes).context("many-facet byte total is invalid")?,
            ));
        }
    }
    Ok(stats)
}

/// Fetch facets for a selected thread page with global item/content budgets and
/// per-row CASE guards. The caller must run `thread_facet_stats_many` first
/// under the same lock to keep aggregate allocation fail-before-read.
pub fn get_facets_many_bounded(
    db: &ProjectionDb,
    thread_ids: &[String],
    max_items: usize,
    max_key_bytes: usize,
    max_value_bytes: usize,
    max_content_bytes: usize,
) -> anyhow::Result<Vec<FacetRow>> {
    if thread_ids.is_empty() {
        return Ok(Vec::new());
    }
    if max_items == 0 {
        anyhow::bail!("many-facet item budget must be positive");
    }
    let mut facets = Vec::new();
    let mut content_bytes = 0usize;
    for batch in thread_ids.chunks(THREAD_ID_QUERY_BATCH) {
        // The CASE guards appear before the IN list in SQL text. Use explicit
        // parameter numbers for the ids: if these were anonymous `?` slots,
        // SQLite would number them after the earlier ?N guards and the actual
        // parameter count would exceed the values supplied for every batch
        // containing more than one thread.
        let placeholders = (1..=batch.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let key_bound_param = batch.len() + 1;
        let value_bound_param = batch.len() + 2;
        let limit_param = batch.len() + 3;
        let sql = format!(
            "SELECT thread_id, \
                    CASE WHEN length(CAST(key AS BLOB)) <= ?{key_bound_param} THEN key ELSE NULL END AS key, \
                    CASE WHEN length(value) <= ?{value_bound_param} THEN value ELSE NULL END AS value, \
                    length(CAST(key AS BLOB)) AS key_len, length(value) AS value_len \
             FROM thread_facets WHERE thread_id IN ({placeholders}) \
             ORDER BY thread_id, key LIMIT ?{limit_param}"
        );
        let remaining = max_items.saturating_sub(facets.len());
        let mut params = batch
            .iter()
            .cloned()
            .map(rusqlite::types::Value::Text)
            .collect::<Vec<_>>();
        params.push(rusqlite::types::Value::Integer(
            i64::try_from(max_key_bytes).unwrap_or(i64::MAX),
        ));
        params.push(rusqlite::types::Value::Integer(
            i64::try_from(max_value_bytes).unwrap_or(i64::MAX),
        ));
        params.push(rusqlite::types::Value::Integer(
            i64::try_from(remaining.saturating_add(1)).unwrap_or(i64::MAX),
        ));
        let mut stmt = db
            .connection()
            .prepare(&sql)
            .context("prepare bounded many-facet query")?;
        let mut rows = stmt
            .query(rusqlite::params_from_iter(params.iter()))
            .context("query bounded many-facet rows")?;
        while let Some(row) = rows.next().context("read bounded many-facet row")? {
            if facets.len() == max_items {
                anyhow::bail!("selected thread facets exceed the {max_items}-item maximum");
            }
            let key_len = usize::try_from(row.get::<_, i64>("key_len")?)
                .context("many-facet key has an invalid length")?;
            let value_len = usize::try_from(row.get::<_, i64>("value_len")?)
                .context("many-facet value has an invalid length")?;
            if key_len > max_key_bytes || value_len > max_value_bytes {
                anyhow::bail!(
                    "selected thread facet exceeds per-entry bounds (key={key_len}/{max_key_bytes}, value={value_len}/{max_value_bytes})"
                );
            }
            content_bytes = content_bytes
                .checked_add(key_len)
                .and_then(|bytes| bytes.checked_add(value_len))
                .context("many-facet content total overflow")?;
            if content_bytes > max_content_bytes {
                anyhow::bail!(
                    "selected thread facet content exceeds the {max_content_bytes}-byte maximum"
                );
            }
            facets.push(FacetRow::from_row(row)?);
        }
    }
    Ok(facets)
}

/// A graph thread's current node: the `(node, step)` of its latest
/// `graph_step_started` event. A cheap per-thread "where is it right now" for a
/// live fleet overview — a single indexed row read, not a full-trace replay.
/// `None` for a thread that has emitted no graph step (non-graph, or not yet
/// started).
pub fn current_graph_node(
    db: &ProjectionDb,
    thread_id: &str,
) -> anyhow::Result<Option<(String, u32)>> {
    let conn = db.connection();
    let mut stmt = conn
        .prepare(
            "SELECT payload FROM events
             WHERE thread_id = ?1 AND event_type = ?2
             ORDER BY thread_seq DESC LIMIT 1",
        )
        .context("prepare current_graph_node")?;
    let mut rows = stmt
        .query_map(
            rusqlite::params![thread_id, crate::event_types::GRAPH_STEP_STARTED],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .context("query current_graph_node")?;
    let Some(row) = rows.next() else {
        return Ok(None);
    };
    let payload: serde_json::Value = serde_json::from_slice(&row.context("read step payload")?)
        .context("decode graph_step_started payload")?;
    let node = payload
        .get("node")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let step = payload
        .get("step")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as u32;
    Ok(node.map(|n| (n, step)))
}

/// Latest graph-step payload per selected thread, fetched in bounded grouped
/// queries. Callers decode after releasing any outer store lock.
pub fn current_graph_node_payloads(
    db: &ProjectionDb,
    thread_ids: &[String],
    max_items: usize,
    max_payload_bytes: usize,
    max_total_bytes: usize,
) -> anyhow::Result<HashMap<String, Vec<u8>>> {
    latest_event_payloads_bounded(
        db,
        thread_ids,
        crate::event_types::GRAPH_STEP_STARTED,
        max_items,
        max_payload_bytes,
        max_total_bytes,
    )
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

/// Node-wide usage settled inside the window (`since_iso`, inclusive) —
/// the pulse's "what did cognition cost lately". Same continuation-aware
/// dedup as the per-chain totals: a settled successor's cumulative row
/// supersedes its predecessors, so reseeded continuations never
/// double-count.
pub fn sum_thread_usage_latest_since(
    db: &ProjectionDb,
    since_iso: &str,
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
         WHERE u.settled_at >= ?"
    );
    db.connection()
        .query_row(&sql, [since_iso], |row| {
            Ok(ThreadUsageTotals {
                thread_count: row.get("thread_count")?,
                completed_turns: row.get("completed_turns")?,
                input_tokens: row.get("input_tokens")?,
                output_tokens: row.get("output_tokens")?,
                spend_usd: row.get("spend_usd")?,
            })
        })
        .context("query sum_thread_usage_latest_since")
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
    use crate::objects::thread_snapshot::{
        CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
        CapturedThreadHistoryPolicy, ThreadHistoryRetention, ThreadSnapshotBuilder, ThreadStatus,
    };
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
        let mut builder = ThreadSnapshotBuilder::new(
            thread_id,
            chain_root_id,
            "directive",
            "system/test",
            "directive-runtime",
        )
        .status(status)
        .upstream_thread_id(upstream_thread_id.map(str::to_string))
        .created_at("2026-06-01T00:00:00Z".to_string());
        builder = match status {
            ThreadStatus::Created => builder.updated_at("2026-06-01T00:00:00Z".to_string()),
            ThreadStatus::Running => builder
                .started_at(Some("2026-06-01T00:00:01Z".to_string()))
                .updated_at("2026-06-01T00:00:01Z".to_string()),
            status if status.is_terminal() => builder
                .started_at(Some("2026-06-01T00:00:01Z".to_string()))
                .finished_at(Some("2026-06-01T00:00:02Z".to_string()))
                .updated_at("2026-06-01T00:00:02Z".to_string()),
            _ => unreachable!("ThreadStatus vocabulary is exhaustive"),
        };
        if thread_id == chain_root_id {
            builder = builder.captured_history_policy(Some(CapturedThreadHistoryPolicy {
                retention: ThreadHistoryRetention::Durable,
                canonical_item_ref: "system/test".to_string(),
                item_content_hash: "11".repeat(32),
                item_signer_fingerprint: Some("22".repeat(32)),
                item_trust_class: CapturedItemTrustClass::Trusted,
                kind_schema_content_hash: "33".repeat(32),
                resolved_from: CapturedPolicyProvenance::NodeDefault {
                    node_policy: CapturedNodeHistoryPolicyProvenance::MissingConfig,
                },
            }));
        }
        let snapshot = builder.build();
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

    /// Project a `thread_continued` edge. `reason`/`fingerprint` are optional;
    /// the OPERATOR path carries `Some("operator_follow_up")` + `Some(fp)`, a
    /// MACHINE link carries a free-form reason + no fingerprint.
    fn project_cont_edge(
        db: &ProjectionDb,
        chain_root_id: &str,
        source_thread_id: &str,
        successor_thread_id: &str,
        chain_seq: u64,
        reason: Option<&str>,
        fingerprint: Option<&str>,
    ) {
        let mut payload = json!({ "successor_thread_id": successor_thread_id });
        if let Some(r) = reason {
            payload["reason"] = json!(r);
        }
        if let Some(fp) = fingerprint {
            payload["successor_request_fingerprint"] = json!(fp);
        }
        let event = NewEvent::new(chain_root_id, source_thread_id, "thread_continued")
            .chain_seq(chain_seq)
            .thread_seq(chain_seq)
            .payload(payload)
            .build_with_ts(format!("2026-06-01T00:{chain_seq:02}:00Z"));
        project_event(db, &event).unwrap();
    }

    #[test]
    fn machine_depth_counts_verified_machine_links() {
        // root → A → B → C, all MACHINE links (reason turn_limit, no fingerprint).
        let db = test_db();
        insert_thread(&db, "root", "K", ThreadStatus::Continued);
        insert_thread_with_upstream(&db, "A", "K", ThreadStatus::Continued, Some("root"));
        insert_thread_with_upstream(&db, "B", "K", ThreadStatus::Continued, Some("A"));
        insert_thread_with_upstream(&db, "C", "K", ThreadStatus::Created, Some("B"));
        project_cont_edge(&db, "K", "root", "A", 1, Some("turn_limit"), None);
        project_cont_edge(&db, "K", "A", "B", 2, Some("turn_limit"), None);
        project_cont_edge(&db, "K", "B", "C", 3, Some("turn_limit"), None);
        assert_eq!(
            consecutive_machine_continuation_depth(&db, "C", 100).unwrap(),
            3
        );
        // The cap bounds the walk.
        assert_eq!(
            consecutive_machine_continuation_depth(&db, "C", 2).unwrap(),
            2
        );
        // A chain root (no upstream) has depth 0.
        assert_eq!(
            consecutive_machine_continuation_depth(&db, "root", 100).unwrap(),
            0
        );
        // An old machine edge with a MISSING reason still counts as machine.
        let db_m = test_db();
        insert_thread(&db_m, "r", "Km", ThreadStatus::Continued);
        insert_thread_with_upstream(&db_m, "s", "Km", ThreadStatus::Created, Some("r"));
        project_cont_edge(&db_m, "Km", "r", "s", 1, None, None);
        assert_eq!(
            consecutive_machine_continuation_depth(&db_m, "s", 100).unwrap(),
            1
        );
    }

    #[test]
    fn machine_depth_ignores_non_continuation_upstream() {
        // `upstream_thread_id` set with NO thread_continued edge (e.g. a
        // compose-context child) must NOT count as a machine continuation.
        let db = test_db();
        insert_thread(&db, "parent", "K", ThreadStatus::Running);
        insert_thread_with_upstream(&db, "child", "K", ThreadStatus::Created, Some("parent"));
        // no continuation edge projected for parent
        assert_eq!(
            consecutive_machine_continuation_depth(&db, "child", 100).unwrap(),
            0
        );

        // An upstream that continued to a DIFFERENT successor must not count for
        // this thread.
        let db2 = test_db();
        insert_thread(&db2, "p", "K2", ThreadStatus::Continued);
        insert_thread_with_upstream(&db2, "x", "K2", ThreadStatus::Created, Some("p"));
        project_cont_edge(&db2, "K2", "p", "OTHER", 1, Some("turn_limit"), None); // p → OTHER, not x
        assert_eq!(
            consecutive_machine_continuation_depth(&db2, "x", 100).unwrap(),
            0
        );
    }

    #[test]
    fn machine_depth_operator_reset_requires_fingerprint() {
        // A VERIFIED operator link (reason + fingerprint) resets the run.
        let db = test_db();
        insert_thread(&db, "r", "K", ThreadStatus::Continued);
        insert_thread_with_upstream(&db, "a", "K", ThreadStatus::Completed, Some("r"));
        insert_thread_with_upstream(&db, "c", "K", ThreadStatus::Created, Some("a"));
        project_cont_edge(
            &db,
            "K",
            "r",
            "a",
            1,
            Some(ContinuationReasonMarker::OperatorFollowUp.as_str()),
            Some("sha256:fp"),
        );
        project_cont_edge(&db, "K", "a", "c", 2, Some("turn_limit"), None);
        // from c: c←a machine (1); a←r operator (verified) → stop. depth 1.
        assert_eq!(
            consecutive_machine_continuation_depth(&db, "c", 100).unwrap(),
            1
        );

        // A machine link SPOOFING reason "operator_follow_up" WITHOUT a fingerprint
        // must NOT reset — it counts as machine, so the cap cannot be bypassed.
        let db2 = test_db();
        insert_thread(&db2, "r", "K2", ThreadStatus::Continued);
        insert_thread_with_upstream(&db2, "a", "K2", ThreadStatus::Continued, Some("r"));
        insert_thread_with_upstream(&db2, "c", "K2", ThreadStatus::Created, Some("a"));
        // spoofed marker, NO fingerprint
        project_cont_edge(
            &db2,
            "K2",
            "r",
            "a",
            1,
            Some(ContinuationReasonMarker::OperatorFollowUp.as_str()),
            None,
        );
        project_cont_edge(&db2, "K2", "a", "c", 2, Some("turn_limit"), None);
        // from c: c←a machine (1); a←r spoofed-operator-no-fp → counts machine (2).
        assert_eq!(
            consecutive_machine_continuation_depth(&db2, "c", 100).unwrap(),
            2
        );
    }

    #[test]
    fn machine_depth_resets_on_graph_follow_resume() {
        // A graph follow-resume edge (daemon-only marker, no fingerprint) is
        // structural progress, not an autonomous run, so it resets the count: a
        // machine edge above it has depth 1, not prior-depth + 1.
        let db = test_db();
        insert_thread(&db, "r", "K", ThreadStatus::Continued);
        insert_thread_with_upstream(&db, "a", "K", ThreadStatus::Continued, Some("r"));
        insert_thread_with_upstream(&db, "c", "K", ThreadStatus::Created, Some("a"));
        project_cont_edge(
            &db,
            "K",
            "r",
            "a",
            1,
            Some(ContinuationReasonMarker::GraphFollowResume.as_str()),
            None,
        );
        project_cont_edge(&db, "K", "a", "c", 2, Some("turn_limit"), None);
        // from c: c←a machine (1); a←r follow-resume → stop. depth 1.
        assert_eq!(
            consecutive_machine_continuation_depth(&db, "c", 100).unwrap(),
            1
        );
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
    fn get_thread_result_rejects_oversized_blob_before_reading_it() {
        let db = test_db();
        let oversized =
            i64::try_from(crate::objects::MAX_THREAD_EVENT_SERIALIZED_BYTES + 1).unwrap();
        db.connection()
            .execute(
                "INSERT INTO thread_results \
                 (thread_id, chain_root_id, status, result, updated_at) \
                 VALUES ('T-large', 'chain-A', 'completed', zeroblob(?1), 'now')",
                [oversized],
            )
            .unwrap();

        let error = get_thread_result(&db, "T-large").unwrap_err();
        assert!(error.to_string().contains("result is"));
        assert!(error.to_string().contains("maximum"));
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
    fn execution_tree_resolves_root_and_combines_spawn_and_continuation_edges() {
        let db = test_db();
        insert_thread(&db, "T-root", "T-root", ThreadStatus::Running);
        insert_thread(&db, "T-child", "T-child", ThreadStatus::Running);
        insert_thread(&db, "T-resume", "T-root", ThreadStatus::Created);
        insert_thread(&db, "T-sub", "T-sub", ThreadStatus::Created);
        db.connection()
            .execute(
                "UPDATE threads SET upstream_thread_id = 'T-root' WHERE thread_id = 'T-resume'",
                [],
            )
            .unwrap();
        project_thread_edge(&db, "T-root", "T-root", "T-child", Some(1), Some("follow")).unwrap();
        project_thread_edge(
            &db,
            "T-root",
            "T-root",
            "T-resume",
            Some(2),
            Some("spawned"),
        )
        .unwrap();
        project_thread_edge(
            &db,
            "T-child",
            "T-child",
            "T-sub",
            Some(1),
            Some("dispatch"),
        )
        .unwrap();

        let rows = execution_tree(&db, "T-sub", 16, 20).unwrap();
        assert_eq!(rows.first().unwrap().thread.thread_id, "T-root");
        let by_id = rows
            .iter()
            .map(|row| (row.thread.thread_id.as_str(), row))
            .collect::<HashMap<_, _>>();
        assert_eq!(by_id["T-child"].relation, "follow");
        assert_eq!(by_id["T-resume"].relation, "continued");
        assert_eq!(by_id["T-sub"].relation, "spawned");
        assert_eq!(
            by_id["T-sub"].tree_parent_thread_id.as_deref(),
            Some("T-child")
        );
        assert_eq!(by_id["T-sub"].depth, 2);
    }

    #[test]
    fn replay_events_filters_by_thread_and_seq() {
        let db = test_db();
        let conn = db.connection();
        conn.execute(
            "INSERT INTO events (event_hash, chain_root_id, chain_seq, thread_id, thread_seq, event_type, durability, ts, payload) \
             VALUES (?, 'chain-A', 1, 'T-1', 0, 'start', 'durable', '2026-01-01T00:00:00Z', X'00')",
            ["a".repeat(64)],
        ).unwrap();
        conn.execute(
            "INSERT INTO events (event_hash, chain_root_id, chain_seq, thread_id, thread_seq, event_type, durability, ts, payload) \
             VALUES (?, 'chain-A', 2, 'T-1', 1, 'step', 'durable', '2026-01-01T00:01:00Z', X'01')",
            ["b".repeat(64)],
        ).unwrap();
        conn.execute(
            "INSERT INTO events (event_hash, chain_root_id, chain_seq, thread_id, thread_seq, event_type, durability, ts, payload) \
             VALUES (?, 'chain-A', 3, 'T-2', 0, 'start', 'durable', '2026-01-01T00:02:00Z', X'02')",
            ["c".repeat(64)],
        ).unwrap();

        let all = replay_events(&db, "chain-A", None, None, 10).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].event_hash, "a".repeat(64));

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
            "INSERT INTO events (event_hash, chain_root_id, chain_seq, thread_id, thread_seq, event_type, durability, ts, payload) \
             VALUES (?, 'chain-A', 1, 'T-1', 0, 'start', 'durable', '2026-01-01T00:00:00Z', X'00')",
            ["d".repeat(64)],
        ).unwrap();
        assert_eq!(
            chain_head_thread(&db, "chain-A").unwrap(),
            Some("T-1".to_string())
        );

        // Chain advances to a successor: the head follows the latest event,
        // independent of insertion timestamps.
        conn.execute(
            "INSERT INTO events (event_hash, chain_root_id, chain_seq, thread_id, thread_seq, event_type, durability, ts, payload) \
             VALUES (?, 'chain-A', 2, 'T-2', 0, 'start', 'durable', '2026-01-01T00:00:00Z', X'01')",
            ["e".repeat(64)],
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
            let event_hash = format!("{seq:064x}");
            conn.execute(
                "INSERT INTO events (event_hash, chain_root_id, chain_seq, thread_id, thread_seq, event_type, durability, ts, payload) \
                 VALUES (?, 'chain-A', ?, 'T-1', ?, 'step', 'durable', '2026-01-01T00:00:00Z', X'00')",
                (&event_hash, seq, seq),
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
        let arts =
            list_thread_artifacts_bounded(&db, "T-1", 512, 1024, 256 * 1024, 4 * 1024 * 1024)
                .unwrap();
        assert!(arts.is_empty());
    }

    #[test]
    fn bounded_many_facets_binds_multiple_thread_ids_before_guard_params() {
        let db = test_db();
        for (thread_id, key, value) in [
            ("T-1", "fleet", b"alpha".as_slice()),
            ("T-2", "team", b"beta".as_slice()),
        ] {
            db.connection()
                .execute(
                    "INSERT INTO thread_facets (thread_id, key, value, updated_at)
                     VALUES (?1, ?2, ?3, '2026-01-01T00:00:00Z')",
                    rusqlite::params![thread_id, key, value],
                )
                .unwrap();
        }

        let thread_ids = vec!["T-1".to_string(), "T-2".to_string()];
        let facets = get_facets_many_bounded(&db, &thread_ids, 8, 32, 32, 128).unwrap();
        assert_eq!(facets.len(), 2);
        assert_eq!(facets[0].thread_id, "T-1");
        assert_eq!(facets[0].key, "fleet");
        assert_eq!(facets[0].value.as_slice(), b"alpha");
        assert_eq!(facets[1].thread_id, "T-2");
        assert_eq!(facets[1].key, "team");
        assert_eq!(facets[1].value.as_slice(), b"beta");
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
        assert_eq!(
            totals.thread_count, 1,
            "completed predecessor must be superseded"
        );
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

    /// Raw insert with a controlled `created_at`, so watch ordering (which lives
    /// in SQL) can be asserted deterministically across timestamps.
    fn insert_thread_at(db: &ProjectionDb, id: &str, status: &str, created_at: &str) {
        db.connection()
            .execute(
                "INSERT INTO threads \
                 (thread_id, chain_root_id, kind, status, item_ref, executor_ref, \
                  launch_mode, current_site_id, origin_site_id, created_at, updated_at) \
                 VALUES (?1, ?1, 'directive', ?2, 'directive:test', 'test/exec', \
                  'inline', 'site:test', 'site:test', ?3, ?3)",
                rusqlite::params![id, status, created_at],
            )
            .unwrap();
    }

    #[test]
    fn watch_sort_orders_active_first_then_newest_limit_after() {
        let db = test_db();
        insert_thread_at(&db, "T-old-run", "running", "2026-01-01T00:00:00Z");
        insert_thread_at(&db, "T-old-done", "completed", "2026-02-01T00:00:00Z");
        insert_thread_at(&db, "T-new-run", "running", "2026-03-01T00:00:00Z");
        insert_thread_at(&db, "T-new-done", "completed", "2026-04-01T00:00:00Z");

        // Watch: active (non-terminal) before terminal, newest-first per bucket.
        let watch = list_threads_sorted(&db, 10, None, ThreadSort::Watch).unwrap();
        assert_eq!(
            watch
                .iter()
                .map(|r| r.thread_id.as_str())
                .collect::<Vec<_>>(),
            ["T-new-run", "T-old-run", "T-new-done", "T-old-done"]
        );

        // Default order is unchanged: oldest-first by created_at.
        let default = list_threads_filtered(&db, 10, None).unwrap();
        assert_eq!(
            default
                .iter()
                .map(|r| r.thread_id.as_str())
                .collect::<Vec<_>>(),
            ["T-old-run", "T-old-done", "T-new-run", "T-new-done"]
        );

        // LIMIT applies AFTER watch ordering — the single row is the top active,
        // not an arbitrary oldest row.
        let top = list_threads_sorted(&db, 1, None, ThreadSort::Watch).unwrap();
        assert_eq!(
            top.iter().map(|r| r.thread_id.as_str()).collect::<Vec<_>>(),
            ["T-new-run"]
        );
    }

    /// Raw insert controlling kind and requested_by too, for filter assertions.
    fn insert_thread_full(
        db: &ProjectionDb,
        id: &str,
        status: &str,
        kind: &str,
        requested_by: &str,
    ) {
        db.connection()
            .execute(
                "INSERT INTO threads \
                 (thread_id, chain_root_id, kind, status, item_ref, executor_ref, \
                  launch_mode, current_site_id, origin_site_id, requested_by, created_at, updated_at) \
                 VALUES (?1, ?1, ?2, ?3, 'directive:test', 'test/exec', \
                  'inline', 'site:test', 'site:test', ?4, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                rusqlite::params![id, kind, status, requested_by],
            )
            .unwrap();
    }

    #[test]
    fn thread_filters_narrow_by_status_kind_source_and_widen_when_unset() {
        let db = test_db();
        insert_thread_full(&db, "T-1", "running", "directive", "fp:claude");
        insert_thread_full(&db, "T-2", "completed", "directive", "fp:amp");
        insert_thread_full(&db, "T-3", "running", "graph", "fp:claude");

        let ids = |rows: Vec<ThreadRow>| {
            let mut v = rows.iter().map(|r| r.thread_id.clone()).collect::<Vec<_>>();
            v.sort();
            v
        };

        // No filter → all rows: an unset filter widens, never empties.
        let all =
            list_threads_query(&db, 10, &ThreadListFilter::default(), ThreadSort::Default).unwrap();
        assert_eq!(ids(all), ["T-1", "T-2", "T-3"]);

        // Each present dashboard filter narrows by substring, so a partial
        // value (as a type-to-filter box produces) still matches.
        let running = list_threads_query(
            &db,
            10,
            &ThreadListFilter {
                status: Some("run".into()),
                ..Default::default()
            },
            ThreadSort::Default,
        )
        .unwrap();
        assert_eq!(ids(running), ["T-1", "T-3"]);

        let graph = list_threads_query(
            &db,
            10,
            &ThreadListFilter {
                kind: Some("graph".into()),
                ..Default::default()
            },
            ThreadSort::Default,
        )
        .unwrap();
        assert_eq!(ids(graph), ["T-3"]);

        let amp = list_threads_query(
            &db,
            10,
            &ThreadListFilter {
                requested_by: Some("fp:amp".into()),
                ..Default::default()
            },
            ThreadSort::Default,
        )
        .unwrap();
        assert_eq!(ids(amp), ["T-2"]);

        // Present filters are ANDed.
        let combined = list_threads_query(
            &db,
            10,
            &ThreadListFilter {
                status: Some("running".into()),
                requested_by: Some("fp:claude".into()),
                ..Default::default()
            },
            ThreadSort::Default,
        )
        .unwrap();
        assert_eq!(ids(combined), ["T-1", "T-3"]);
    }
}
