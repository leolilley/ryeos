//! Query layer for the rye-state projection database.
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

// ============= Query functions =============

const THREAD_COLUMNS: &str = r#"
    thread_id, chain_root_id, kind, status,
    item_ref, executor_ref, launch_mode,
    current_site_id, origin_site_id, upstream_thread_id, requested_by,
    created_at, updated_at, started_at, finished_at
"#;

pub fn get_thread(db: &ProjectionDb, thread_id: &str) -> anyhow::Result<Option<ThreadRow>> {
    let sql = &format!(
        "SELECT {THREAD_COLUMNS} FROM threads WHERE thread_id = ?"
    );
    let mut stmt = db
        .connection()
        .prepare(sql)
        .context("prepare get_thread")?;
    stmt.query_row([thread_id], ThreadRow::from_row)
        .optional()
        .context("query get_thread")
}

pub fn list_threads_by_chain(db: &ProjectionDb, chain_root_id: &str) -> anyhow::Result<Vec<ThreadRow>> {
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

pub fn list_threads_by_status(db: &ProjectionDb, statuses: &[&str]) -> anyhow::Result<Vec<ThreadRow>> {
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
    let sql = &format!(
        "SELECT {THREAD_COLUMNS} FROM threads ORDER BY created_at LIMIT ?"
    );
    let mut stmt = db
        .connection()
        .prepare(sql)
        .context("prepare list_threads")?;
    let rows = stmt
        .query_map([limit], ThreadRow::from_row)
        .context("query list_threads")?;
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
        .query_row(&sql, params.as_slice(), |row| row.get(0))
        .context("query active_thread_count")?;
    Ok(count)
}

pub fn get_thread_result(db: &ProjectionDb, thread_id: &str) -> anyhow::Result<Option<ThreadResultRow>> {
    let mut stmt = db
        .connection()
        .prepare(
            "SELECT thread_id, chain_root_id, status, result, error, updated_at \
             FROM thread_results WHERE thread_id = ?",
        )
        .context("prepare get_thread_result")?;
    stmt.query_row([thread_id], ThreadResultRow::from_row)
        .optional()
        .context("query get_thread_result")
}

pub fn list_thread_artifacts(db: &ProjectionDb, thread_id: &str) -> anyhow::Result<Vec<ArtifactRow>> {
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

pub fn list_thread_edges(db: &ProjectionDb, chain_root_id: &str) -> anyhow::Result<Vec<ThreadEdgeRow>> {
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
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(chain_root_id.to_string())];

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

// ============= Tests =============

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::thread_snapshot::{ThreadSnapshotBuilder, ThreadStatus};
    use crate::projection::{project_thread_edge, project_thread_snapshot};

    fn test_db() -> ProjectionDb {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("test.db");
        ProjectionDb::open(&path).unwrap()
    }

    fn insert_thread(db: &ProjectionDb, thread_id: &str, chain_root_id: &str, status: ThreadStatus) {
        let snapshot = ThreadSnapshotBuilder::new(
            thread_id,
            chain_root_id,
            "directive",
            "system/test",
            "directive-runtime",
        )
        .status(status)
        .build();
        project_thread_snapshot(db, &snapshot, chain_root_id).unwrap();
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

        project_thread_edge(&db, "chain-A", "T-parent", "T-child1", Some(1), Some("spawn"))
            .unwrap();
        project_thread_edge(&db, "chain-A", "T-parent", "T-child2", Some(2), None)
            .unwrap();

        let children = list_thread_children(&db, "T-parent").unwrap();
        assert_eq!(children.len(), 2);
        let ids: Vec<&str> = children.iter().map(|c| c.thread_id.as_str()).collect();
        assert!(ids.contains(&"T-child1"));
        assert!(ids.contains(&"T-child2"));
    }

    #[test]
    fn list_thread_edges_by_chain() {
        let db = test_db();
        project_thread_edge(&db, "chain-A", "T-p", "T-c1", Some(1), Some("reason"))
            .unwrap();
        project_thread_edge(&db, "chain-A", "T-p", "T-c2", None, None)
            .unwrap();

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
    fn list_thread_artifacts_empty() {
        let db = test_db();
        let arts = list_thread_artifacts(&db, "T-1").unwrap();
        assert!(arts.is_empty());
    }
}
