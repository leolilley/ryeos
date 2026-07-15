use anyhow::Context;
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::OptionalExtension;

use super::ProjectionDb;
use crate::objects::{
    parse_canonical_timestamp, CapturedThreadHistoryPolicy, ThreadHistoryRetention,
    MAX_TERMINAL_DURATION_SECONDS,
};

const TERMINAL_STATUSES_SQL: &str =
    "'completed','failed','cancelled','killed','timed_out','continued'";

#[derive(Debug, Clone)]
pub(crate) struct TerminalMember {
    pub is_terminal: bool,
    pub timestamp: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DerivedTerminalRetention {
    pub terminal_at: Option<String>,
    pub retire_after: Option<i64>,
}

/// Derive terminality and its deadline from authoritative member timestamps.
///
/// Every member timestamp is parsed before terminality is considered. This
/// prevents non-canonical or malformed values from changing the maximum instant
/// in either the projection or the destructive authoritative path.
pub(crate) fn derive_terminal_retention(
    members: impl IntoIterator<Item = TerminalMember>,
    retention: &ThreadHistoryRetention,
) -> anyhow::Result<DerivedTerminalRetention> {
    let mut count = 0usize;
    let mut all_terminal = true;
    let mut latest: Option<DateTime<Utc>> = None;
    for member in members {
        count += 1;
        all_terminal &= member.is_terminal;
        let timestamp = parse_canonical_timestamp(&member.timestamp).with_context(|| {
            format!(
                "invalid canonical retention timestamp `{}`",
                member.timestamp
            )
        })?;
        if latest.as_ref().map_or(true, |current| timestamp > *current) {
            latest = Some(timestamp);
        }
    }
    if count == 0 {
        anyhow::bail!("cannot derive retention for a chain without thread members");
    }
    if !all_terminal {
        return Ok(DerivedTerminalRetention {
            terminal_at: None,
            retire_after: None,
        });
    }
    let terminal_at = latest.expect("non-empty member timestamps");
    let retire_after = match retention {
        ThreadHistoryRetention::TerminalFor { seconds } => {
            if *seconds == 0 || *seconds > MAX_TERMINAL_DURATION_SECONDS {
                anyhow::bail!(
                    "captured history terminal retention must be within 1..={MAX_TERMINAL_DURATION_SECONDS} seconds"
                );
            }
            let seconds = i64::try_from(*seconds)
                .context("captured history retention duration exceeds i64")?;
            let deadline = terminal_at
                .timestamp()
                .checked_add(seconds)
                .ok_or_else(|| anyhow::anyhow!("terminal retention deadline is out of range"))?;
            Some(deadline)
        }
        ThreadHistoryRetention::Durable => None,
    };
    Ok(DerivedTerminalRetention {
        terminal_at: Some(terminal_at.to_rfc3339_opts(SecondsFormat::Secs, true)),
        retire_after,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueTerminalChain {
    pub chain_root_id: String,
    pub indexed_chain_state_hash: String,
    pub terminal_at: String,
    pub retire_after: i64,
    pub root_updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueTerminalChainCursor {
    pub retire_after: i64,
    pub chain_root_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainRetentionProjection {
    pub chain_root_id: String,
    pub captured_policy: CapturedThreadHistoryPolicy,
    pub terminal_at: Option<String>,
    pub retire_after: Option<i64>,
}

pub(crate) fn refresh_chain_retention(
    db: &ProjectionDb,
    chain_root_id: &str,
) -> anyhow::Result<()> {
    let root: Option<(Option<String>,)> = db
        .connection()
        .query_row(
            "SELECT captured_history_policy_json FROM threads \
             WHERE thread_id=?1 AND chain_root_id=?1",
            [chain_root_id],
            |row| Ok((row.get(0)?,)),
        )
        .optional()
        .context("query root captured history policy")?;
    let Some((policy_json,)) = root else {
        db.connection().execute(
            "DELETE FROM chain_retention WHERE chain_root_id=?1",
            [chain_root_id],
        )?;
        return Ok(());
    };

    let policy_json = policy_json.ok_or_else(|| {
        anyhow::anyhow!("projected chain root {chain_root_id} is missing captured_history_policy")
    })?;
    let captured_policy: CapturedThreadHistoryPolicy =
        serde_json::from_str(&policy_json).context("decode projected captured history policy")?;
    let mut statement = db
        .connection()
        .prepare("SELECT status, finished_at, updated_at FROM threads WHERE chain_root_id=?1")?;
    let members = statement
        .query_map([chain_root_id], |row| {
            let status: String = row.get(0)?;
            let finished_at: Option<String> = row.get(1)?;
            let updated_at: String = row.get(2)?;
            Ok(TerminalMember {
                is_terminal: matches!(
                    status.as_str(),
                    "completed" | "failed" | "cancelled" | "killed" | "timed_out" | "continued"
                ),
                timestamp: finished_at.unwrap_or(updated_at),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let derived = derive_terminal_retention(members, &captured_policy.retention)?;

    db.connection().execute(
        "INSERT OR REPLACE INTO chain_retention (chain_root_id, terminal_at, retire_after) \
         VALUES (?1, ?2, ?3)",
        rusqlite::params![chain_root_id, derived.terminal_at, derived.retire_after],
    )?;
    Ok(())
}

impl ProjectionDb {
    pub fn list_due_terminal_chains(
        &self,
        now: &str,
        limit: usize,
        after: Option<&DueTerminalChainCursor>,
    ) -> anyhow::Result<Vec<DueTerminalChain>> {
        let limit = i64::try_from(limit).context("due-chain limit exceeds i64")?;
        let now = parse_canonical_timestamp(now)?.timestamp();
        let after_deadline = after.map(|cursor| cursor.retire_after);
        let after_root = after.map(|cursor| cursor.chain_root_id.as_str());
        let mut statement = self.connection().prepare(&format!(
            "SELECT root.chain_root_id, meta.indexed_chain_state_hash, retention.terminal_at, retention.retire_after, root.updated_at \
             FROM chain_retention retention JOIN threads root \
               ON root.thread_id=retention.chain_root_id AND root.chain_root_id=retention.chain_root_id \
             JOIN projection_meta meta ON meta.chain_root_id=retention.chain_root_id \
             WHERE retention.retire_after IS NOT NULL AND retention.retire_after <= ?1 \
               AND retention.terminal_at IS NOT NULL \
               AND (?2 IS NULL OR retention.retire_after > ?2 OR (retention.retire_after = ?2 AND retention.chain_root_id > ?3)) \
               AND NOT EXISTS (SELECT 1 FROM threads member \
                   WHERE member.chain_root_id=retention.chain_root_id \
                     AND member.status NOT IN ({TERMINAL_STATUSES_SQL})) \
             ORDER BY retention.retire_after, retention.chain_root_id LIMIT ?4"
        ))?;
        let rows = statement.query_map(
            rusqlite::params![now, after_deadline, after_root, limit],
            |row| {
                Ok(DueTerminalChain {
                    chain_root_id: row.get(0)?,
                    indexed_chain_state_hash: row.get(1)?,
                    terminal_at: row.get(2)?,
                    retire_after: row.get(3)?,
                    root_updated_at: row.get(4)?,
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn chain_retention_projection(
        &self,
        chain_root_id: &str,
    ) -> anyhow::Result<Option<ChainRetentionProjection>> {
        let row: Option<(Option<String>, Option<String>, Option<i64>)> = self
            .connection()
            .query_row(
                "SELECT root.captured_history_policy_json, retention.terminal_at, retention.retire_after \
                 FROM threads root LEFT JOIN chain_retention retention \
                   ON retention.chain_root_id=root.chain_root_id \
                 WHERE root.thread_id=?1 AND root.chain_root_id=?1",
                [chain_root_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        row.map(|(policy_json, terminal_at, retire_after)| {
            let policy_json = policy_json.ok_or_else(|| {
                anyhow::anyhow!(
                    "projected chain root {chain_root_id} is missing captured_history_policy"
                )
            })?;
            let captured_policy = serde_json::from_str(&policy_json)
                .context("decode projected captured history policy")?;
            Ok(ChainRetentionProjection {
                chain_root_id: chain_root_id.to_string(),
                captured_policy,
                terminal_at,
                retire_after,
            })
        })
        .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::thread_snapshot::{
        CapturedEffectiveTrustClass, CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance,
        CapturedPolicyProvenance, ThreadSnapshotBuilder, ThreadStatus,
    };

    fn captured_policy(seconds: u64) -> CapturedThreadHistoryPolicy {
        let hash = "a".repeat(64);
        CapturedThreadHistoryPolicy {
            retention: ThreadHistoryRetention::TerminalFor { seconds },
            canonical_item_ref: "directive:test".to_string(),
            item_content_hash: hash.clone(),
            item_signer_fingerprint: Some(hash.clone()),
            item_trust_class: CapturedItemTrustClass::Trusted,
            kind_schema_content_hash: hash,
            resolved_from: CapturedPolicyProvenance::ItemAuthored {
                composed_path: "history".to_string(),
                requested_seconds: seconds,
                effective_trust_class: CapturedEffectiveTrustClass::TrustedBundle,
                minimum_clamp: None,
                node_policy: CapturedNodeHistoryPolicyProvenance::MissingConfig,
            },
        }
    }

    #[test]
    fn deadline_uses_whole_chain_terminal_time() {
        let temp = tempfile::tempdir().unwrap();
        let db = ProjectionDb::open(&temp.path().join("projection.sqlite3")).unwrap();
        let root = ThreadSnapshotBuilder::new(
            "T-root",
            "T-root",
            "directive",
            "directive:test",
            "directive-runtime",
        )
        .captured_history_policy(Some(captured_policy(60)))
        .status(ThreadStatus::Completed)
        .created_at("2026-01-01T00:00:00Z".to_string())
        .updated_at("2026-01-01T00:00:00Z".to_string())
        .finished_at(Some("2026-01-01T00:00:00Z".to_string()))
        .build();
        crate::projection::project_thread_snapshot(&db, &root, "T-root").unwrap();

        let running_child = ThreadSnapshotBuilder::new(
            "T-child",
            "T-root",
            "directive",
            "directive:test",
            "directive-runtime",
        )
        .status(ThreadStatus::Running)
        .created_at("2026-01-01T00:00:00Z".to_string())
        .started_at(Some("2026-01-01T00:02:00Z".to_string()))
        .updated_at("2026-01-01T00:02:00Z".to_string())
        .build();
        crate::projection::project_thread_snapshot(&db, &running_child, "T-root").unwrap();
        assert!(db
            .chain_retention_projection("T-root")
            .unwrap()
            .unwrap()
            .retire_after
            .is_none());

        let terminal_child = ThreadSnapshotBuilder::new(
            "T-child",
            "T-root",
            "directive",
            "directive:test",
            "directive-runtime",
        )
        .status(ThreadStatus::Completed)
        .created_at("2026-01-01T00:00:00Z".to_string())
        .updated_at("2026-01-01T00:02:00Z".to_string())
        .finished_at(Some("2026-01-01T00:02:00Z".to_string()))
        .build();
        crate::projection::project_thread_snapshot(&db, &terminal_child, "T-root").unwrap();
        let retention = db.chain_retention_projection("T-root").unwrap().unwrap();
        assert_eq!(
            retention.terminal_at.as_deref(),
            Some("2026-01-01T00:02:00Z")
        );
        assert_eq!(retention.retire_after, Some(1_767_225_780));
    }

    #[test]
    fn malformed_member_time_fails_closed_even_when_chain_is_nonterminal() {
        let error = derive_terminal_retention(
            [TerminalMember {
                is_terminal: false,
                timestamp: "not-a-time".to_string(),
            }],
            &ThreadHistoryRetention::Durable,
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("invalid canonical retention timestamp"));
    }

    #[test]
    fn noncanonical_member_time_fails_closed() {
        for timestamp in ["2026-01-01T00:30:00+01:00", "2026-01-01T00:00:00.500Z"] {
            let error = derive_terminal_retention(
                [TerminalMember {
                    is_terminal: true,
                    timestamp: timestamp.to_string(),
                }],
                &ThreadHistoryRetention::TerminalFor { seconds: 60 },
            )
            .unwrap_err();
            assert!(error
                .to_string()
                .contains("invalid canonical retention timestamp"));
        }
    }
}
