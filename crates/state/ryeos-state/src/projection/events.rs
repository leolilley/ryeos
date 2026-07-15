use anyhow::Context;

use super::ProjectionDb;

#[derive(Debug)]
pub(crate) struct ProjectionEventConflict {
    event_hash: String,
    chain_root_id: String,
    chain_seq: u64,
}

impl std::fmt::Display for ProjectionEventConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "projection event conflict for chain {} sequence {} (authoritative hash {})",
            self.chain_root_id, self.chain_seq, self.event_hash,
        )
    }
}

impl std::error::Error for ProjectionEventConflict {}

/// Project a thread event into the events table.
///
/// Called when durable events are appended to the chain. For
/// `artifact_published` events, also derives an artifact row from
/// the event payload.
#[tracing::instrument(
    level = "debug",
    name = "state:project_event",
    skip(db, event),
    fields(
        thread_id = %event.thread_id,
        event_type = %event.event_type,
    )
)]
pub fn project_event(db: &ProjectionDb, event: &crate::ThreadEvent) -> anyhow::Result<()> {
    event.validate()?;

    let payload =
        serde_json::to_vec(&event.payload).context("failed to serialize event payload")?;
    let event_hash = lillux::sha256_hex(lillux::canonical_json(&event.to_value()).as_bytes());
    let durability = event.durability.to_string();

    db.connection()
        .execute(
            "INSERT OR IGNORE INTO events (
            event_hash, chain_root_id, chain_seq, thread_id, thread_seq,
            event_type, durability, ts, prev_chain_event_hash,
            prev_thread_event_hash, payload
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                &event_hash,
                &event.chain_root_id,
                event.chain_seq,
                &event.thread_id,
                event.thread_seq,
                &event.event_type,
                &durability,
                &event.ts,
                &event.prev_chain_event_hash,
                &event.prev_thread_event_hash,
                &payload,
            ],
        )
        .context("failed to project event")?;

    // INSERT OR IGNORE is used only for semantic idempotency. A uniqueness
    // conflict at either the event hash or authoritative chain sequence must
    // already be the exact same canonical event; otherwise derived rows and
    // the projection cursor must not advance over a stale/forked base row.
    let exact: bool = db
        .connection()
        .query_row(
            "SELECT EXISTS (
                SELECT 1 FROM events
                WHERE event_hash = ?1
                  AND chain_root_id = ?2
                  AND chain_seq = ?3
                  AND thread_id = ?4
                  AND thread_seq = ?5
                  AND event_type = ?6
                  AND durability = ?7
                  AND ts = ?8
                  AND prev_chain_event_hash IS ?9
                  AND prev_thread_event_hash IS ?10
                  AND payload = ?11
            )",
            rusqlite::params![
                &event_hash,
                &event.chain_root_id,
                event.chain_seq,
                &event.thread_id,
                event.thread_seq,
                &event.event_type,
                &durability,
                &event.ts,
                &event.prev_chain_event_hash,
                &event.prev_thread_event_hash,
                &payload,
            ],
            |row| row.get(0),
        )
        .context("verify idempotent projected event")?;
    if !exact {
        return Err(ProjectionEventConflict {
            event_hash,
            chain_root_id: event.chain_root_id.clone(),
            chain_seq: event.chain_seq,
        }
        .into());
    }

    // Derive artifact row from artifact_published events (CAS-truth derived)
    if event.event_type == crate::event_types::ARTIFACT_PUBLISHED {
        if let Some(artifact_type) = event.payload.get("artifact_type").and_then(|v| v.as_str()) {
            let metadata = event.payload.get("metadata").cloned();
            let metadata_blob = metadata
                .map(|m| serde_json::to_vec(&m).context("failed to serialize metadata"))
                .transpose()?;

            db.connection()
                .execute(
                    "INSERT INTO thread_artifacts (
                    source_event_hash, chain_root_id, thread_id, kind, metadata, created_at
                ) VALUES (?, ?, ?, ?, ?, ?)
                 ON CONFLICT(source_event_hash) DO UPDATE SET
                    chain_root_id = excluded.chain_root_id,
                    thread_id = excluded.thread_id,
                    kind = excluded.kind,
                    metadata = excluded.metadata,
                    created_at = excluded.created_at",
                    rusqlite::params![
                        &event_hash,
                        &event.chain_root_id,
                        &event.thread_id,
                        artifact_type,
                        metadata_blob,
                        &event.ts,
                    ],
                )
                .context("failed to project derived artifact")?;
        }
    }

    if event.event_type == crate::event_types::THREAD_USAGE {
        project_thread_usage_latest(db, event)?;
    }

    if event.event_type == crate::event_types::THREAD_CREATED {
        project_thread_usage_subject(db, event)?;
    }

    // Derive a dispatch thread-edge from a child_thread_spawned event. An
    // inline-dispatched directive/sub-graph child is a FRESH ROOT with no
    // upstream_thread_id, so the snapshot-derived edge above never links it; this
    // event carries the only portable parent→child lineage. The canonical
    // source-event hash is the semantic idempotency key, so replay reasserts
    // the exact derived row instead of duplicating it. The emitting thread is
    // the parent.
    if event.event_type == crate::event_types::CHILD_THREAD_SPAWNED {
        if let Some(child_id) = event
            .payload
            .get("child_thread_id")
            .and_then(|v| v.as_str())
        {
            let spawn_reason = event
                .payload
                .get("spawn_reason")
                .and_then(|v| v.as_str())
                .unwrap_or("dispatch");
            db.connection()
                .execute(
                    "INSERT INTO thread_edges (
                        chain_root_id, parent_thread_id, child_thread_id, spawn_seq,
                        spawn_reason, source_event_hash, created_at
                    ) VALUES (?, ?, ?, NULL, ?, ?, ?)
                    ON CONFLICT(source_event_hash) DO UPDATE SET
                        chain_root_id = excluded.chain_root_id,
                        parent_thread_id = excluded.parent_thread_id,
                        child_thread_id = excluded.child_thread_id,
                        spawn_seq = excluded.spawn_seq,
                        spawn_reason = excluded.spawn_reason,
                        created_at = excluded.created_at",
                    rusqlite::params![
                        &event.chain_root_id,
                        &event.thread_id,
                        child_id,
                        spawn_reason,
                        &event_hash,
                        &event.ts,
                    ],
                )
                .context("failed to project dispatch thread edge")?;
        }
    }

    // Derive a facet row from a thread_facet_set event. Facets MUST originate in
    // an event (not a bare table write) to survive a projection rebuild, which
    // clears thread_facets and re-derives it from the event log. Upsert on
    // (thread_id, key): the latest set wins.
    if event.event_type == crate::event_types::THREAD_FACET_SET {
        if let (Some(key), Some(value)) = (
            event.payload.get("key").and_then(|v| v.as_str()),
            event.payload.get("value").and_then(|v| v.as_str()),
        ) {
            db.connection()
                .execute(
                    "INSERT INTO thread_facets (thread_id, key, value, updated_at)
                     VALUES (?, ?, ?, ?)
                     ON CONFLICT(thread_id, key)
                     DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                    rusqlite::params![&event.thread_id, key, value.as_bytes(), &event.ts,],
                )
                .context("failed to project thread facet")?;
        }
    }

    Ok(())
}

fn project_thread_usage_subject(
    db: &ProjectionDb,
    event: &crate::ThreadEvent,
) -> anyhow::Result<()> {
    let Some(subject_value) = event.payload.get("usage_subject") else {
        return Ok(());
    };

    let usage_subject: crate::UsageSubject = serde_json::from_value(subject_value.clone())
        .context("failed to deserialize thread_created usage_subject")?;
    usage_subject.validate()?;
    let asserted_by = event
        .payload
        .get("usage_subject_asserted_by")
        .and_then(|value| value.as_str())
        .map(str::to_string);

    db.connection()
        .execute(
            "INSERT INTO thread_usage_subjects (
                chain_root_id, namespace, subject, asserted_by, created_at
            ) VALUES (?, ?, ?, ?, ?)
            ON CONFLICT(chain_root_id) DO UPDATE SET
                namespace = excluded.namespace,
                subject = excluded.subject,
                asserted_by = excluded.asserted_by,
                created_at = excluded.created_at",
            rusqlite::params![
                &event.chain_root_id,
                &usage_subject.namespace,
                &usage_subject.subject,
                asserted_by,
                &event.ts,
            ],
        )
        .context("failed to project thread_usage_subjects")?;

    Ok(())
}

fn project_thread_usage_latest(
    db: &ProjectionDb,
    event: &crate::ThreadEvent,
) -> anyhow::Result<()> {
    let usage: crate::ThreadUsage =
        serde_json::from_value(event.payload.clone()).context("invalid thread_usage payload")?;
    usage.validate()?;

    db.connection()
        .execute(
            "INSERT INTO thread_usage_latest (
                thread_id, chain_root_id, chain_seq, thread_seq,
                completed_turns, input_tokens, output_tokens, spend_usd, spawns_used,
                started_at, settled_at, last_settled_turn_seq, elapsed_ms,
                provider_id, model, profile
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(thread_id) DO UPDATE SET
                chain_root_id = excluded.chain_root_id,
                chain_seq = excluded.chain_seq,
                thread_seq = excluded.thread_seq,
                completed_turns = excluded.completed_turns,
                input_tokens = excluded.input_tokens,
                output_tokens = excluded.output_tokens,
                spend_usd = excluded.spend_usd,
                spawns_used = excluded.spawns_used,
                started_at = excluded.started_at,
                settled_at = excluded.settled_at,
                last_settled_turn_seq = excluded.last_settled_turn_seq,
                elapsed_ms = excluded.elapsed_ms,
                provider_id = excluded.provider_id,
                model = excluded.model,
                profile = excluded.profile
            WHERE excluded.chain_seq > thread_usage_latest.chain_seq",
            rusqlite::params![
                &event.thread_id,
                &event.chain_root_id,
                u64_to_i64(event.chain_seq, "chain_seq")?,
                u64_to_i64(event.thread_seq, "thread_seq")?,
                i64::from(usage.completed_turns),
                u64_to_i64(usage.input_tokens, "input_tokens")?,
                u64_to_i64(usage.output_tokens, "output_tokens")?,
                usage.spend_usd,
                i64::from(usage.spawns_used),
                usage.started_at,
                usage.settled_at,
                u64_to_i64(usage.last_settled_turn_seq, "last_settled_turn_seq")?,
                u64_to_i64(usage.elapsed_ms, "elapsed_ms")?,
                usage.provider_id,
                usage.model,
                usage.profile,
            ],
        )
        .context("failed to project thread_usage_latest")?;

    Ok(())
}

fn u64_to_i64(value: u64, field: &str) -> anyhow::Result<i64> {
    i64::try_from(value).with_context(|| format!("thread_usage {field} exceeds i64"))
}

/// Project a thread edge (parent-child relationship).
///
/// Called when a child thread is spawned.
pub fn project_thread_edge(
    db: &ProjectionDb,
    chain_root_id: &str,
    parent_thread_id: &str,
    child_thread_id: &str,
    spawn_seq: Option<i64>,
    spawn_reason: Option<&str>,
) -> anyhow::Result<()> {
    tracing::trace!(
        chain_root_id = %chain_root_id,
        parent_thread_id = %parent_thread_id,
        child_thread_id = %child_thread_id,
        spawn_reason = spawn_reason.unwrap_or(""),
        "project thread edge"
    );
    db.connection()
        .execute(
            "INSERT INTO thread_edges (
            chain_root_id, parent_thread_id, child_thread_id, spawn_seq,
            spawn_reason, source_event_hash, created_at
        ) VALUES (?, ?, ?, ?, ?, NULL, ?)",
            rusqlite::params![
                chain_root_id,
                parent_thread_id,
                child_thread_id,
                spawn_seq,
                spawn_reason,
                lillux::time::iso8601_now(),
            ],
        )
        .context("failed to project edge")?;

    Ok(())
}
