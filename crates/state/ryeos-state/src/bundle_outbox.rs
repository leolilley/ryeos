//! Transactional outbox helpers for bundle event projections.

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::bundle_events::BundleEventRecord;
use crate::objects::validate_bundle_identifier;

const OUTBOX_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS bundle_outbox_messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    projection_name TEXT NOT NULL,
    sink TEXT NOT NULL,
    event_hash TEXT NOT NULL,
    bundle_id TEXT NOT NULL,
    event_kind TEXT NOT NULL,
    event_type TEXT NOT NULL,
    chain_id TEXT NOT NULL,
    chain_seq INTEGER NOT NULL CHECK (chain_seq > 0),
    payload_json TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'in_progress', 'delivered', 'failed')),
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at TEXT NOT NULL,
    leased_by TEXT,
    lease_until TEXT,
    delivered_at TEXT,
    last_error TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (projection_name, sink, event_hash)
);

CREATE INDEX IF NOT EXISTS idx_bundle_outbox_claim
    ON bundle_outbox_messages(status, next_attempt_at, id);

CREATE INDEX IF NOT EXISTS idx_bundle_outbox_event
    ON bundle_outbox_messages(event_hash);
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleOutboxMessage {
    pub id: i64,
    pub projection_name: String,
    pub sink: String,
    pub event_hash: String,
    pub bundle_id: String,
    pub event_kind: String,
    pub event_type: String,
    pub chain_id: String,
    pub chain_seq: u64,
    pub payload_json: String,
    pub status: String,
    pub attempts: u32,
    pub next_attempt_at: String,
    pub leased_by: Option<String>,
    pub lease_until: Option<String>,
    pub delivered_at: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub fn ensure_bundle_outbox_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(OUTBOX_SCHEMA_SQL)
        .context("create bundle outbox schema")
}

pub fn enqueue_bundle_outbox_message(
    conn: &Connection,
    projection_name: &str,
    sink: &str,
    record: &BundleEventRecord,
    payload_json: &serde_json::Value,
) -> anyhow::Result<()> {
    validate_bundle_identifier("projection_name", projection_name)?;
    validate_bundle_identifier("sink", sink)?;
    let now = lillux::time::iso8601_now();
    let payload_json = serde_json::to_string(payload_json)?;
    conn.execute(
        "INSERT INTO bundle_outbox_messages (
            projection_name, sink, event_hash, bundle_id, event_kind, event_type,
            chain_id, chain_seq, payload_json, status, next_attempt_at, created_at, updated_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?)
         ON CONFLICT(projection_name, sink, event_hash) DO NOTHING",
        params![
            projection_name,
            sink,
            record.event_hash,
            record.event.bundle_id,
            record.event.event_kind,
            record.event.event_type,
            record.event.chain_id,
            record.event.chain_seq as i64,
            payload_json,
            now,
            now,
            now,
        ],
    )?;
    Ok(())
}

pub fn claim_bundle_outbox_messages(
    conn: &mut Connection,
    projection_name: &str,
    sink: &str,
    worker_id: &str,
    limit: usize,
    lease_until: &str,
    max_attempts: Option<u32>,
) -> anyhow::Result<Vec<BundleOutboxMessage>> {
    validate_bundle_identifier("projection_name", projection_name)?;
    validate_bundle_identifier("sink", sink)?;
    validate_bundle_identifier("worker_id", worker_id)?;
    if limit == 0 {
        return Ok(Vec::new());
    }
    if lease_until.is_empty() {
        anyhow::bail!("lease_until must not be empty");
    }
    if max_attempts == Some(0) {
        anyhow::bail!("max_attempts must be greater than zero");
    }
    let now = lillux::time::iso8601_now();
    let max_attempts_i64 = max_attempts.map(i64::from);
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(max_attempts) = max_attempts_i64 {
        tx.execute(
            "UPDATE bundle_outbox_messages
             SET status = 'failed',
                 leased_by = NULL,
                 lease_until = NULL,
                 last_error = COALESCE(last_error, 'max attempts exhausted'),
                 updated_at = ?
             WHERE projection_name = ?
               AND sink = ?
               AND attempts >= ?
               AND (
                    status = 'pending'
                    OR (status = 'in_progress' AND lease_until IS NOT NULL AND lease_until <= ?)
               )",
            params![now, projection_name, sink, max_attempts, now],
        )?;
    }
    let ids = {
        let mut stmt = tx.prepare(
            "SELECT id
             FROM bundle_outbox_messages
             WHERE projection_name = ?
               AND sink = ?
               AND (? IS NULL OR attempts < ?)
               AND (
                    (status = 'pending' AND next_attempt_at <= ?)
                    OR (status = 'in_progress' AND lease_until IS NOT NULL AND lease_until <= ?)
               )
             ORDER BY next_attempt_at, id
             LIMIT ?",
        )?;
        let ids = stmt
            .query_map(
                params![
                    projection_name,
                    sink,
                    max_attempts_i64,
                    max_attempts_i64,
                    now,
                    now,
                    limit as i64
                ],
                |row| row.get::<_, i64>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        ids
    };

    for id in &ids {
        tx.execute(
            "UPDATE bundle_outbox_messages
             SET status = 'in_progress',
                 attempts = attempts + 1,
                 leased_by = ?,
                 lease_until = ?,
                 updated_at = ?
             WHERE id = ?
               AND projection_name = ?
               AND sink = ?
               AND (? IS NULL OR attempts < ?)
               AND (
                    (status = 'pending' AND next_attempt_at <= ?)
                    OR (status = 'in_progress' AND lease_until IS NOT NULL AND lease_until <= ?)
               )",
            params![
                worker_id,
                lease_until,
                now,
                id,
                projection_name,
                sink,
                max_attempts_i64,
                max_attempts_i64,
                now,
                now,
            ],
        )?;
    }

    let mut messages = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(message) = select_outbox_message(&tx, id)? {
            if message.status == "in_progress"
                && message.leased_by.as_deref() == Some(worker_id)
                && message.lease_until.as_deref() == Some(lease_until)
            {
                messages.push(message);
            }
        }
    }
    tx.commit()?;
    Ok(messages)
}

pub fn mark_bundle_outbox_delivered(
    conn: &Connection,
    id: i64,
    worker_id: &str,
    attempts: u32,
) -> anyhow::Result<bool> {
    validate_bundle_identifier("worker_id", worker_id)?;
    let now = lillux::time::iso8601_now();
    let updated = conn.execute(
        "UPDATE bundle_outbox_messages
         SET status = 'delivered',
             delivered_at = ?,
             leased_by = NULL,
             lease_until = NULL,
             updated_at = ?
         WHERE id = ?
           AND status = 'in_progress'
           AND leased_by = ?
           AND attempts = ?",
        params![now, now, id, worker_id, attempts as i64],
    )?;
    Ok(updated == 1)
}

pub fn mark_bundle_outbox_failed(
    conn: &Connection,
    id: i64,
    worker_id: &str,
    attempts: u32,
    next_attempt_at: Option<&str>,
    last_error: Option<&str>,
    max_attempts: Option<u32>,
) -> anyhow::Result<bool> {
    validate_bundle_identifier("worker_id", worker_id)?;
    let now = lillux::time::iso8601_now();
    let exhausted = max_attempts.is_some_and(|max| attempts >= max);
    let status = if next_attempt_at.is_some() && !exhausted {
        "pending"
    } else {
        "failed"
    };
    let next_attempt_at = next_attempt_at.unwrap_or(&now);
    let updated = conn.execute(
        "UPDATE bundle_outbox_messages
         SET status = ?,
             next_attempt_at = ?,
             leased_by = NULL,
             lease_until = NULL,
             last_error = ?,
             updated_at = ?
         WHERE id = ?
           AND status = 'in_progress'
           AND leased_by = ?
           AND attempts = ?",
        params![
            status,
            next_attempt_at,
            last_error,
            now,
            id,
            worker_id,
            attempts as i64
        ],
    )?;
    Ok(updated == 1)
}

pub fn get_bundle_outbox_message(
    conn: &Connection,
    id: i64,
) -> anyhow::Result<Option<BundleOutboxMessage>> {
    select_outbox_message(conn, id)
}

fn select_outbox_message(
    conn: &Connection,
    id: i64,
) -> anyhow::Result<Option<BundleOutboxMessage>> {
    conn.query_row(
        "SELECT id, projection_name, sink, event_hash, bundle_id, event_kind,
                event_type, chain_id, chain_seq, payload_json, status, attempts,
                next_attempt_at, leased_by, lease_until, delivered_at, last_error,
                created_at, updated_at
         FROM bundle_outbox_messages
         WHERE id = ?",
        params![id],
        |row| {
            Ok(BundleOutboxMessage {
                id: row.get(0)?,
                projection_name: row.get(1)?,
                sink: row.get(2)?,
                event_hash: row.get(3)?,
                bundle_id: row.get(4)?,
                event_kind: row.get(5)?,
                event_type: row.get(6)?,
                chain_id: row.get(7)?,
                chain_seq: row.get::<_, i64>(8)? as u64,
                payload_json: row.get(9)?,
                status: row.get(10)?,
                attempts: row.get::<_, i64>(11)? as u32,
                next_attempt_at: row.get(12)?,
                leased_by: row.get(13)?,
                lease_until: row.get(14)?,
                delivered_at: row.get(15)?,
                last_error: row.get(16)?,
                created_at: row.get(17)?,
                updated_at: row.get(18)?,
            })
        },
    )
    .optional()
    .context("read bundle outbox message")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle_events::{append_bundle_event, BundleEventAppendRequest};
    use crate::signer::TestSigner;

    fn append_request(chain_id: &str) -> BundleEventAppendRequest {
        BundleEventAppendRequest {
            effective_bundle_id: "ryeos-email".to_string(),
            bundle_id: Some("ryeos-email".to_string()),
            event_kind: "email_event".to_string(),
            chain_id: chain_id.to_string(),
            event_type: "email_ready".to_string(),
            schema_version: 1,
            payload: serde_json::json!({"email_id": chain_id}),
            expected_chain_head_hash: None,
            idempotency_key: None,
            correlation_id: None,
            causation_id: None,
            attribution: Default::default(),
        }
    }

    #[test]
    fn outbox_enqueue_claim_deliver_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();
        let signer = TestSigner::default();
        let appended =
            append_bundle_event(&cas_root, &refs_root, append_request("email_1"), &signer).unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        ensure_bundle_outbox_schema(&conn).unwrap();
        let record = BundleEventRecord {
            event_hash: appended.event_hash.clone(),
            event: appended.event.clone(),
        };
        enqueue_bundle_outbox_message(
            &conn,
            "ryeos-email",
            "smtp",
            &record,
            &serde_json::json!({"to": "user@example.com"}),
        )
        .unwrap();
        enqueue_bundle_outbox_message(
            &conn,
            "ryeos-email",
            "smtp",
            &record,
            &serde_json::json!({"to": "user@example.com"}),
        )
        .unwrap();

        let claimed = claim_bundle_outbox_messages(
            &mut conn,
            "ryeos-email",
            "smtp",
            "worker_1",
            10,
            "9999-01-01T00:00:00Z",
            None,
        )
        .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].attempts, 1);
        assert_eq!(claimed[0].event_hash, appended.event_hash);

        assert!(mark_bundle_outbox_delivered(
            &conn,
            claimed[0].id,
            "worker_1",
            claimed[0].attempts
        )
        .unwrap());
        let delivered = get_bundle_outbox_message(&conn, claimed[0].id)
            .unwrap()
            .unwrap();
        assert_eq!(delivered.status, "delivered");
        assert!(delivered.delivered_at.is_some());
    }

    #[test]
    fn outbox_failed_with_retry_becomes_pending_again() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();
        let signer = TestSigner::default();
        let appended =
            append_bundle_event(&cas_root, &refs_root, append_request("email_2"), &signer).unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        ensure_bundle_outbox_schema(&conn).unwrap();
        let record = BundleEventRecord {
            event_hash: appended.event_hash.clone(),
            event: appended.event.clone(),
        };
        enqueue_bundle_outbox_message(
            &conn,
            "ryeos-email",
            "smtp",
            &record,
            &serde_json::json!({"to": "user@example.com"}),
        )
        .unwrap();
        let claimed = claim_bundle_outbox_messages(
            &mut conn,
            "ryeos-email",
            "smtp",
            "worker_1",
            1,
            "9999-01-01T00:00:00Z",
            None,
        )
        .unwrap();

        assert!(mark_bundle_outbox_failed(
            &conn,
            claimed[0].id,
            "worker_1",
            claimed[0].attempts,
            Some("0000-01-01T00:00:00Z"),
            Some("smtp unavailable"),
            None,
        )
        .unwrap());
        let retried = claim_bundle_outbox_messages(
            &mut conn,
            "ryeos-email",
            "smtp",
            "worker_2",
            1,
            "9999-01-01T00:00:00Z",
            None,
        )
        .unwrap();
        assert_eq!(retried.len(), 1);
        assert_eq!(retried[0].attempts, 2);
        assert_eq!(retried[0].leased_by.as_deref(), Some("worker_2"));
    }

    #[test]
    fn outbox_stale_worker_cannot_finalize_reclaimed_message() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();
        let signer = TestSigner::default();
        let appended =
            append_bundle_event(&cas_root, &refs_root, append_request("email_3"), &signer).unwrap();
        let record = BundleEventRecord {
            event_hash: appended.event_hash.clone(),
            event: appended.event.clone(),
        };

        let mut conn = Connection::open_in_memory().unwrap();
        ensure_bundle_outbox_schema(&conn).unwrap();
        enqueue_bundle_outbox_message(
            &conn,
            "ryeos-email",
            "smtp",
            &record,
            &serde_json::json!({"to": "user@example.com"}),
        )
        .unwrap();

        let first_claim = claim_bundle_outbox_messages(
            &mut conn,
            "ryeos-email",
            "smtp",
            "worker_1",
            1,
            "0000-01-01T00:00:00Z",
            None,
        )
        .unwrap();
        let second_claim = claim_bundle_outbox_messages(
            &mut conn,
            "ryeos-email",
            "smtp",
            "worker_2",
            1,
            "9999-01-01T00:00:00Z",
            None,
        )
        .unwrap();

        assert_eq!(second_claim.len(), 1);
        assert!(!mark_bundle_outbox_delivered(
            &conn,
            first_claim[0].id,
            "worker_1",
            first_claim[0].attempts
        )
        .unwrap());
        assert!(mark_bundle_outbox_delivered(
            &conn,
            second_claim[0].id,
            "worker_2",
            second_claim[0].attempts
        )
        .unwrap());
    }

    #[test]
    fn outbox_max_attempts_moves_failure_to_terminal_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();
        let signer = TestSigner::default();
        let appended =
            append_bundle_event(&cas_root, &refs_root, append_request("email_4"), &signer).unwrap();
        let record = BundleEventRecord {
            event_hash: appended.event_hash.clone(),
            event: appended.event.clone(),
        };

        let mut conn = Connection::open_in_memory().unwrap();
        ensure_bundle_outbox_schema(&conn).unwrap();
        enqueue_bundle_outbox_message(
            &conn,
            "ryeos-email",
            "smtp",
            &record,
            &serde_json::json!({"to": "user@example.com"}),
        )
        .unwrap();
        let claimed = claim_bundle_outbox_messages(
            &mut conn,
            "ryeos-email",
            "smtp",
            "worker_1",
            1,
            "9999-01-01T00:00:00Z",
            Some(1),
        )
        .unwrap();

        assert!(mark_bundle_outbox_failed(
            &conn,
            claimed[0].id,
            "worker_1",
            claimed[0].attempts,
            Some("0000-01-01T00:00:00Z"),
            Some("smtp unavailable"),
            Some(1),
        )
        .unwrap());
        let failed = get_bundle_outbox_message(&conn, claimed[0].id)
            .unwrap()
            .unwrap();
        assert_eq!(failed.status, "failed");
        let reclaimed = claim_bundle_outbox_messages(
            &mut conn,
            "ryeos-email",
            "smtp",
            "worker_2",
            1,
            "9999-01-01T00:00:00Z",
            Some(1),
        )
        .unwrap();
        assert!(reclaimed.is_empty());
    }

    #[test]
    fn outbox_expired_final_attempt_is_marked_failed_on_next_claim() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();
        let signer = TestSigner::default();
        let appended =
            append_bundle_event(&cas_root, &refs_root, append_request("email_5"), &signer).unwrap();
        let record = BundleEventRecord {
            event_hash: appended.event_hash.clone(),
            event: appended.event.clone(),
        };

        let mut conn = Connection::open_in_memory().unwrap();
        ensure_bundle_outbox_schema(&conn).unwrap();
        enqueue_bundle_outbox_message(
            &conn,
            "ryeos-email",
            "smtp",
            &record,
            &serde_json::json!({"to": "user@example.com"}),
        )
        .unwrap();
        let claimed = claim_bundle_outbox_messages(
            &mut conn,
            "ryeos-email",
            "smtp",
            "worker_1",
            1,
            "0000-01-01T00:00:00Z",
            Some(1),
        )
        .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].attempts, 1);

        let reclaimed = claim_bundle_outbox_messages(
            &mut conn,
            "ryeos-email",
            "smtp",
            "worker_2",
            1,
            "9999-01-01T00:00:00Z",
            Some(1),
        )
        .unwrap();
        assert!(reclaimed.is_empty());
        let failed = get_bundle_outbox_message(&conn, claimed[0].id)
            .unwrap()
            .unwrap();
        assert_eq!(failed.status, "failed");
        assert_eq!(failed.leased_by, None);
        assert_eq!(failed.lease_until, None);
    }
}
