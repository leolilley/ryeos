use anyhow::Context;

use super::ProjectionDb;

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
                event.durability.to_string(),
                &event.ts,
                &event.prev_chain_event_hash,
                &event.prev_thread_event_hash,
                &payload,
            ],
        )
        .context("failed to project event")?;

    // Derive artifact row from artifact_published events (CAS-truth derived)
    if event.event_type == crate::event_types::ARTIFACT_PUBLISHED {
        if let Some(artifact_type) = event.payload.get("artifact_type").and_then(|v| v.as_str()) {
            let metadata = event.payload.get("metadata").cloned();
            let metadata_blob = metadata
                .map(|m| serde_json::to_vec(&m).context("failed to serialize metadata"))
                .transpose()?;

            db.connection()
                .execute(
                    "INSERT OR IGNORE INTO thread_artifacts (
                    chain_root_id, thread_id, kind, metadata, created_at
                ) VALUES (?, ?, ?, ?, ?)",
                    rusqlite::params![
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
    // event carries the only portable parent→child lineage. Rebuild-safe (the
    // edge re-derives from the event on projection rebuild); INSERT OR IGNORE
    // keeps re-projection idempotent. The emitting thread is the parent.
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
                    "INSERT OR IGNORE INTO thread_edges (
                        chain_root_id, parent_thread_id, child_thread_id, spawn_seq, spawn_reason, created_at
                    ) VALUES (?, ?, ?, NULL, ?, ?)",
                    rusqlite::params![
                        &event.chain_root_id,
                        &event.thread_id,
                        child_id,
                        spawn_reason,
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
    let completed_turns = payload_u64_to_i64(&event.payload, "completed_turns")?;
    let input_tokens = payload_u64_to_i64(&event.payload, "input_tokens")?;
    let output_tokens = payload_u64_to_i64(&event.payload, "output_tokens")?;
    let spawns_used = payload_u64_to_i64(&event.payload, "spawns_used")?;
    let last_settled_turn_seq = payload_u64_to_i64(&event.payload, "last_settled_turn_seq")?;
    let elapsed_ms = payload_u64_to_i64(&event.payload, "elapsed_ms")?;
    let started_at = payload_str(&event.payload, "started_at")?;
    let settled_at = payload_str(&event.payload, "settled_at")?;
    let spend_usd = event
        .payload
        .get("spend_usd")
        .and_then(|value| value.as_f64())
        .context("thread_usage payload missing numeric spend_usd")?;
    let provider_id = event
        .payload
        .get("provider_id")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let model = event
        .payload
        .get("model")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let profile = event
        .payload
        .get("profile")
        .and_then(|value| value.as_str())
        .map(str::to_string);

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
                completed_turns,
                input_tokens,
                output_tokens,
                spend_usd,
                spawns_used,
                started_at,
                settled_at,
                last_settled_turn_seq,
                elapsed_ms,
                provider_id,
                model,
                profile,
            ],
        )
        .context("failed to project thread_usage_latest")?;

    Ok(())
}

fn payload_u64_to_i64(payload: &serde_json::Value, field: &str) -> anyhow::Result<i64> {
    let value = payload
        .get(field)
        .and_then(|value| value.as_u64())
        .with_context(|| format!("thread_usage payload missing integer {field}"))?;
    u64_to_i64(value, field)
}

fn payload_str(payload: &serde_json::Value, field: &str) -> anyhow::Result<String> {
    payload
        .get(field)
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .with_context(|| format!("thread_usage payload missing string {field}"))
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
            chain_root_id, parent_thread_id, child_thread_id, spawn_seq, spawn_reason, created_at
        ) VALUES (?, ?, ?, ?, ?, ?)",
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
