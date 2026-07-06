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

/// # Follow lineage: the two projected edge kinds (and the one that is NOT)
///
/// A graph `follow:` relationship spans two distinct lineage links. Only the
/// first is recorded in the projection today; the second is NOT, by current
/// design:
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
/// 2. **Parent → followed child chain root (cross-chain, NOT projected).** The
///    followed child is spawned as a FRESH ROOT — its own `chain_root_id`, no
///    `upstream_thread_id` — so `project_thread_snapshot` derives no edge, and
///    the parent↔child link lives ONLY in the operational `follow_waiter` table
///    (runtime_db), never in CAS-derived projection data. Once the waiter is
///    cleared, that historical "this thread followed into child chain X" fact is
///    gone from durable state.
///
/// Recording kind (2) durably would require emitting a new cross-chain spawn
/// event from the follow-spawn path (executor side) through the event/projection
/// pipeline AND extending the chain-scoped `thread_edges` model to hold a
/// cross-chain edge — a deliberate change deferred to the wave that owns the
/// follow spawn/event path (it pairs with nested child-braid rendering). Until
/// then, a client reads live follow lineage from the waiter-sourced `follow`
/// fact on a thread projection, and terminal-history resume lineage from kind (1)
/// above; the cross-chain child link is not queryable from the projection.
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

    pub fn from_str(value: &str) -> Option<Self> {
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
        Self::from_str(value).is_some()
    }
}

/// The continuation EDGE on a source's `thread_continued` payload, if any:
/// `(successor_thread_id, reason, request_fingerprint)`. The fingerprint is
/// present only on OPERATOR follow-ups (`create_or_get_continuation` records it);
/// a machine continuation never does, so it cannot spoof the operator marker by
/// passing `reason == "operator_follow_up"`. Returns `None` when there is no
/// `thread_continued` event or its payload names no successor.
pub fn continuation_edge(
    db: &ProjectionDb,
    thread_id: &str,
) -> anyhow::Result<Option<(String, Option<String>, Option<String>)>> {
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
        "SELECT event_id, chain_root_id, chain_seq, thread_id, thread_seq, \
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
