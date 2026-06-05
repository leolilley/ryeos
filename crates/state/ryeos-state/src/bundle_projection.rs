//! Rebuildable SQLite projection helpers for bundle events.

use std::path::Path;

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::bundle_events::BundleEventRecord;
use crate::objects::validate_bundle_identifier;

const META_SCHEMA_SQL: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;

CREATE TABLE IF NOT EXISTS bundle_projection_meta (
    projection_name TEXT PRIMARY KEY,
    schema_version INTEGER NOT NULL CHECK (schema_version > 0),
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS bundle_projection_cursors (
    projection_name TEXT NOT NULL,
    bundle_id TEXT NOT NULL,
    event_kind TEXT NOT NULL,
    chain_id TEXT NOT NULL,
    last_chain_seq INTEGER NOT NULL CHECK (last_chain_seq >= 0),
    last_event_hash TEXT,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (projection_name, bundle_id, event_kind, chain_id)
);

CREATE INDEX IF NOT EXISTS idx_bundle_projection_cursors_projection
    ON bundle_projection_cursors(projection_name);
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleProjectionCursor {
    pub projection_name: String,
    pub bundle_id: String,
    pub event_kind: String,
    pub chain_id: String,
    pub last_chain_seq: u64,
    pub last_event_hash: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleProjectionSyncReport {
    pub projection_name: String,
    pub schema_version: u32,
    pub scanned: usize,
    pub projected: usize,
    pub skipped: usize,
}

pub struct BundleProjectionDb {
    conn: Connection,
}

impl BundleProjectionDb {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create projection dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("open bundle projection db {}", path.display()))?;
        conn.execute_batch(META_SCHEMA_SQL)
            .context("create bundle projection metadata schema")?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        Ok(Self { conn })
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    pub fn apply_schema(&self, schema_sql: &str) -> anyhow::Result<()> {
        self.conn
            .execute_batch(schema_sql)
            .context("apply bundle projection schema")
    }

    pub fn reset_projection(
        &self,
        projection_name: &str,
        schema_version: u32,
    ) -> anyhow::Result<()> {
        validate_projection_name(projection_name)?;
        if schema_version == 0 {
            anyhow::bail!("schema_version must be greater than zero");
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM bundle_projection_cursors WHERE projection_name = ?",
            params![projection_name],
        )?;
        tx.execute(
            "INSERT INTO bundle_projection_meta (projection_name, schema_version, updated_at)
             VALUES (?, ?, ?)
             ON CONFLICT(projection_name) DO UPDATE SET
                schema_version = excluded.schema_version,
                updated_at = excluded.updated_at",
            params![projection_name, schema_version, lillux::time::iso8601_now()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn cursor(
        &self,
        projection_name: &str,
        bundle_id: &str,
        event_kind: &str,
        chain_id: &str,
    ) -> anyhow::Result<Option<BundleProjectionCursor>> {
        validate_projection_name(projection_name)?;
        validate_bundle_identifier("bundle_id", bundle_id)?;
        validate_bundle_identifier("event_kind", event_kind)?;
        validate_bundle_identifier("chain_id", chain_id)?;
        self.conn
            .query_row(
                "SELECT projection_name, bundle_id, event_kind, chain_id,
                        last_chain_seq, last_event_hash, updated_at
                 FROM bundle_projection_cursors
                 WHERE projection_name = ? AND bundle_id = ? AND event_kind = ? AND chain_id = ?",
                params![projection_name, bundle_id, event_kind, chain_id],
                |row| {
                    Ok(BundleProjectionCursor {
                        projection_name: row.get(0)?,
                        bundle_id: row.get(1)?,
                        event_kind: row.get(2)?,
                        chain_id: row.get(3)?,
                        last_chain_seq: row.get::<_, i64>(4)? as u64,
                        last_event_hash: row.get(5)?,
                        updated_at: row.get(6)?,
                    })
                },
            )
            .optional()
            .context("read bundle projection cursor")
    }

    pub fn sync_records<F>(
        &mut self,
        projection_name: &str,
        schema_version: u32,
        records: &[BundleEventRecord],
        mut handler: F,
    ) -> anyhow::Result<BundleProjectionSyncReport>
    where
        F: FnMut(&Connection, &BundleEventRecord) -> anyhow::Result<()>,
    {
        validate_projection_name(projection_name)?;
        if schema_version == 0 {
            anyhow::bail!("schema_version must be greater than zero");
        }

        let mut scanned = 0usize;
        let mut projected = 0usize;
        let mut skipped = 0usize;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_projection_meta(&tx, projection_name, schema_version)?;

        for record in records {
            scanned += 1;
            let current = cursor_in_tx(
                &tx,
                projection_name,
                &record.event.bundle_id,
                &record.event.event_kind,
                &record.event.chain_id,
            )?;
            if let Some(cursor) = &current {
                if record.event.chain_seq < cursor.last_chain_seq {
                    skipped += 1;
                    continue;
                }
                if record.event.chain_seq == cursor.last_chain_seq {
                    if cursor.last_event_hash.as_deref() != Some(record.event_hash.as_str()) {
                        anyhow::bail!(
                            "bundle projection cursor hash mismatch for {}/{}/{} at seq {}",
                            record.event.bundle_id,
                            record.event.event_kind,
                            record.event.chain_id,
                            record.event.chain_seq
                        );
                    }
                    skipped += 1;
                    continue;
                }
                if record.event.chain_seq != cursor.last_chain_seq + 1 {
                    anyhow::bail!(
                        "bundle projection cursor gap for {}/{}/{}: cursor={}, event={}",
                        record.event.bundle_id,
                        record.event.event_kind,
                        record.event.chain_id,
                        cursor.last_chain_seq,
                        record.event.chain_seq
                    );
                }
                if record.event.prev_chain_event_hash.as_deref()
                    != cursor.last_event_hash.as_deref()
                {
                    anyhow::bail!(
                        "bundle projection cursor link mismatch for {}/{}/{}: cursor_hash={:?}, event_prev={:?}",
                        record.event.bundle_id,
                        record.event.event_kind,
                        record.event.chain_id,
                        cursor.last_event_hash,
                        record.event.prev_chain_event_hash
                    );
                }
            } else if record.event.chain_seq != 1 {
                anyhow::bail!(
                    "bundle projection missing chain start for {}/{}/{} at seq {}",
                    record.event.bundle_id,
                    record.event.event_kind,
                    record.event.chain_id,
                    record.event.chain_seq
                );
            } else if record.event.prev_chain_event_hash.is_some() {
                anyhow::bail!(
                    "bundle projection chain start for {}/{}/{} has prev hash {:?}",
                    record.event.bundle_id,
                    record.event.event_kind,
                    record.event.chain_id,
                    record.event.prev_chain_event_hash
                );
            }
            handler(&tx, record)?;
            upsert_cursor_in_tx(&tx, projection_name, record)?;
            projected += 1;
        }

        tx.commit()?;
        Ok(BundleProjectionSyncReport {
            projection_name: projection_name.to_string(),
            schema_version,
            scanned,
            projected,
            skipped,
        })
    }
}

fn ensure_projection_meta(
    conn: &Connection,
    projection_name: &str,
    schema_version: u32,
) -> anyhow::Result<()> {
    let existing: Option<u32> = conn
        .query_row(
            "SELECT schema_version FROM bundle_projection_meta WHERE projection_name = ?",
            params![projection_name],
            |row| row.get::<_, i64>(0).map(|v| v as u32),
        )
        .optional()?;
    match existing {
        Some(existing) if existing != schema_version => anyhow::bail!(
            "bundle projection schema_version drift for {}: existing={}, requested={}",
            projection_name,
            existing,
            schema_version
        ),
        Some(_) => Ok(()),
        None => {
            conn.execute(
                "INSERT INTO bundle_projection_meta (projection_name, schema_version, updated_at)
                 VALUES (?, ?, ?)",
                params![projection_name, schema_version, lillux::time::iso8601_now()],
            )?;
            Ok(())
        }
    }
}

fn cursor_in_tx(
    conn: &Connection,
    projection_name: &str,
    bundle_id: &str,
    event_kind: &str,
    chain_id: &str,
) -> anyhow::Result<Option<BundleProjectionCursor>> {
    conn.query_row(
        "SELECT projection_name, bundle_id, event_kind, chain_id,
                last_chain_seq, last_event_hash, updated_at
         FROM bundle_projection_cursors
         WHERE projection_name = ? AND bundle_id = ? AND event_kind = ? AND chain_id = ?",
        params![projection_name, bundle_id, event_kind, chain_id],
        |row| {
            Ok(BundleProjectionCursor {
                projection_name: row.get(0)?,
                bundle_id: row.get(1)?,
                event_kind: row.get(2)?,
                chain_id: row.get(3)?,
                last_chain_seq: row.get::<_, i64>(4)? as u64,
                last_event_hash: row.get(5)?,
                updated_at: row.get(6)?,
            })
        },
    )
    .optional()
    .context("read bundle projection cursor")
}

fn upsert_cursor_in_tx(
    conn: &Connection,
    projection_name: &str,
    record: &BundleEventRecord,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO bundle_projection_cursors (
            projection_name, bundle_id, event_kind, chain_id,
            last_chain_seq, last_event_hash, updated_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(projection_name, bundle_id, event_kind, chain_id) DO UPDATE SET
            last_chain_seq = excluded.last_chain_seq,
            last_event_hash = excluded.last_event_hash,
            updated_at = excluded.updated_at",
        params![
            projection_name,
            record.event.bundle_id,
            record.event.event_kind,
            record.event.chain_id,
            record.event.chain_seq as i64,
            record.event_hash,
            lillux::time::iso8601_now(),
        ],
    )?;
    Ok(())
}

fn validate_projection_name(value: &str) -> anyhow::Result<()> {
    validate_bundle_identifier("projection_name", value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle_events::{append_bundle_event, BundleEventAppendRequest};
    use crate::signer::TestSigner;

    fn append_request(chain_id: &str, event_type: &str) -> BundleEventAppendRequest {
        BundleEventAppendRequest {
            effective_bundle_id: "ryeos-email".to_string(),
            bundle_id: Some("ryeos-email".to_string()),
            event_kind: "email_event".to_string(),
            chain_id: chain_id.to_string(),
            event_type: event_type.to_string(),
            schema_version: 1,
            payload: serde_json::json!({"email_id": chain_id, "event_type": event_type}),
            expected_chain_head_hash: None,
            idempotency_key: None,
            correlation_id: None,
            causation_id: None,
            attribution: Default::default(),
        }
    }

    #[test]
    fn projection_sync_tracks_per_chain_cursors_and_skips_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();
        let signer = TestSigner::default();

        let first = append_bundle_event(
            &cas_root,
            &refs_root,
            append_request("email_1", "email_planned"),
            &signer,
        )
        .unwrap();
        let mut second_req = append_request("email_1", "email_approved");
        second_req.expected_chain_head_hash = Some(first.event_hash.clone());
        append_bundle_event(&cas_root, &refs_root, second_req, &signer).unwrap();

        let records = crate::bundle_events::scan_bundle_events(
            &cas_root,
            &refs_root,
            "ryeos-email",
            "email_event",
        )
        .unwrap();
        let mut projection = BundleProjectionDb::open(&tmp.path().join("email.db")).unwrap();
        projection
            .apply_schema(
                "CREATE TABLE email_events (
                    event_hash TEXT PRIMARY KEY,
                    chain_id TEXT NOT NULL,
                    chain_seq INTEGER NOT NULL,
                    event_type TEXT NOT NULL
                );",
            )
            .unwrap();

        let report = projection
            .sync_records("ryeos-email", 1, &records, |conn, record| {
                conn.execute(
                    "INSERT INTO email_events (event_hash, chain_id, chain_seq, event_type)
                     VALUES (?, ?, ?, ?)",
                    params![
                        record.event_hash,
                        record.event.chain_id,
                        record.event.chain_seq as i64,
                        record.event.event_type,
                    ],
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(report.projected, 2);

        let replay = projection
            .sync_records("ryeos-email", 1, &records, |_conn, _record| {
                panic!("already-projected records should be skipped")
            })
            .unwrap();
        assert_eq!(replay.skipped, 2);
        assert_eq!(replay.projected, 0);

        let cursor = projection
            .cursor("ryeos-email", "ryeos-email", "email_event", "email_1")
            .unwrap()
            .unwrap();
        assert_eq!(cursor.last_chain_seq, 2);
    }

    #[test]
    fn projection_rejects_schema_version_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let mut projection = BundleProjectionDb::open(&tmp.path().join("email.db")).unwrap();
        let records = Vec::<BundleEventRecord>::new();
        projection
            .sync_records("ryeos-email", 1, &records, |_conn, _record| Ok(()))
            .unwrap();
        let err = projection
            .sync_records("ryeos-email", 2, &records, |_conn, _record| Ok(()))
            .unwrap_err();
        assert!(format!("{err:#}").contains("schema_version drift"));
    }

    #[test]
    fn projection_rejects_replayed_sequence_with_different_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();
        let signer = TestSigner::default();
        let appended = append_bundle_event(
            &cas_root,
            &refs_root,
            append_request("email_1", "email_planned"),
            &signer,
        )
        .unwrap();

        let record = BundleEventRecord {
            event_hash: appended.event_hash.clone(),
            event: appended.event.clone(),
        };
        let mut projection = BundleProjectionDb::open(&tmp.path().join("email.db")).unwrap();
        projection
            .sync_records(
                "ryeos-email",
                1,
                std::slice::from_ref(&record),
                |_conn, _record| Ok(()),
            )
            .unwrap();

        let mut bad_record = record;
        bad_record.event_hash = "0".repeat(64);
        let err = projection
            .sync_records("ryeos-email", 1, &[bad_record], |_conn, _record| Ok(()))
            .unwrap_err();
        assert!(format!("{err:#}").contains("cursor hash mismatch"));
    }

    #[test]
    fn projection_rejects_next_sequence_with_wrong_prev_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();
        let signer = TestSigner::default();

        let first = append_bundle_event(
            &cas_root,
            &refs_root,
            append_request("email_1", "email_planned"),
            &signer,
        )
        .unwrap();
        let mut second_req = append_request("email_1", "email_approved");
        second_req.expected_chain_head_hash = Some(first.event_hash.clone());
        append_bundle_event(&cas_root, &refs_root, second_req, &signer).unwrap();
        let records = crate::bundle_events::scan_bundle_events(
            &cas_root,
            &refs_root,
            "ryeos-email",
            "email_event",
        )
        .unwrap();

        let mut projection = BundleProjectionDb::open(&tmp.path().join("email.db")).unwrap();
        projection
            .sync_records("ryeos-email", 1, &records[..1], |_conn, _record| Ok(()))
            .unwrap();

        let mut bad_second = records[1].clone();
        bad_second.event.prev_chain_event_hash = Some("0".repeat(64));
        let err = projection
            .sync_records("ryeos-email", 1, &[bad_second], |_conn, _record| Ok(()))
            .unwrap_err();
        assert!(format!("{err:#}").contains("cursor link mismatch"));
    }
}
