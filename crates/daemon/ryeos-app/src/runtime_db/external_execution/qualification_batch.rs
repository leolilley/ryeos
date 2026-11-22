//! Durable, session-bound pushed ObservationBatch ingress for one Q Worker.
//!
//! Batches describe the entire Q session, including setup. Their retained
//! request digest identifies the command row; it does not attribute every
//! event to the turn. Setup may retain session events and remote-thread
//! bindings, but State observations require actual turn contact first.
//!
//! A batch ACK means its complete canonical content and chain coordinate were
//! committed locally. It proves neither causality nor a semantic result.
//! Later transcript retention preserves this session evidence for a verifier;
//! it opens no positive qualification claim. Request-correlated Delta/Final
//! observations have their own command journal.

use super::*;
use anyhow::ensure;
use serde::Deserialize;
use serde_json::Value;

use super::qualification_command::{QualificationWorkerCommandPhase, read_command, require_owner};

const MAX_BATCH_BYTES: usize = ryeos_state::objects::MAX_STRUCTURED_OBSERVATION_BATCH_BYTES;
const MAX_JOURNAL_BYTES: i64 = 64 * 1024 * 1024;
const MAX_WIRE_BYTES: u32 = 16 * 1024 * 1024;
const MAX_OBSERVATIONS_PER_EVENT: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QualificationWorkerBatchAck {
    pub through_sequence: u64,
    pub batch_digest: String,
}

/// Full retained ingress for a later Q semantic verifier. This is evidence,
/// never a success permit or a reason to infer a completed turn.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct QualificationWorkerObservationBatchRecord {
    pub placement_thread_id: String,
    pub process_incarnation_digest: String,
    pub request_digest: String,
    pub first_sequence: u64,
    pub through_sequence: u64,
    pub previous_digest: Option<String>,
    pub batch_digest: String,
    pub batch: Value,
    pub wire_max_bytes: u32,
    pub retained_at_ms: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerObservationBatch {
    first_sequence: u64,
    count: u64,
    previous_digest: Option<String>,
    batch_digest: String,
    events: Vec<Value>,
    session_observations: Vec<Value>,
}

struct PreparedBatch {
    first_sequence: u64,
    through_sequence: u64,
    previous_digest: Option<String>,
    batch_digest: String,
    canonical_json: String,
}

fn require_next_link(
    prior: Option<(i64, &str, i64)>,
    batch: &PreparedBatch,
    wire_max_bytes: u32,
) -> Result<()> {
    let first = i64::try_from(batch.first_sequence)?;
    match prior {
        Some((prior_through, prior_digest, prior_wire)) => ensure!(
            first
                == prior_through
                    .checked_add(1)
                    .context("Q sequence overflow")?
                && batch.previous_digest.as_deref() == Some(prior_digest)
                && i64::from(wire_max_bytes) == prior_wire,
            "Q observation batch is not the next exact chain link"
        ),
        None => ensure!(
            first == 1 && batch.previous_digest.is_none(),
            "Q observation batch does not start its chain at one"
        ),
    }
    Ok(())
}

fn require_exact_replay(
    existing: &QualificationWorkerObservationBatchRecord,
    process_incarnation_digest: &str,
    request_digest: &str,
    batch: &PreparedBatch,
    raw: &Value,
    wire_max_bytes: u32,
) -> Result<()> {
    ensure!(
        existing.process_incarnation_digest == process_incarnation_digest
            && existing.request_digest == request_digest
            && existing.first_sequence == batch.first_sequence
            && existing.through_sequence == batch.through_sequence
            && existing.previous_digest == batch.previous_digest
            && existing.batch_digest == batch.batch_digest
            && existing.batch == *raw
            && existing.wire_max_bytes == wire_max_bytes,
        "Q observation sequence identity was replayed with different content"
    );
    Ok(())
}

fn prepare_batch(raw: &Value, wire_max_bytes: u32) -> Result<PreparedBatch> {
    ensure!(
        (1..=MAX_WIRE_BYTES).contains(&wire_max_bytes),
        "Q Worker observation has no bounded selected wire maximum"
    );
    ensure!(
        serde_json::to_vec(raw)?.len() <= MAX_BATCH_BYTES,
        "Q Worker observation batch exceeds its serialized-byte ceiling"
    );
    let canonical_json = lillux::canonical_json(raw)?;
    ensure!(
        canonical_json.len() <= MAX_BATCH_BYTES && canonical_json.len() <= wire_max_bytes as usize,
        "Q Worker observation batch exceeds its exact signed or structural byte bound"
    );
    let mut digest_input = raw.clone();
    let supplied_digest = digest_input
        .as_object_mut()
        .and_then(|object| object.remove("batch_digest"))
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .context("Q Worker observation batch has no digest")?;
    let computed_digest = ryeos_state::objects::canonical_value_digest(&digest_input)?;
    ensure!(
        supplied_digest == computed_digest,
        "Q Worker observation batch canonical digest differs"
    );
    let batch: WorkerObservationBatch =
        serde_json::from_value(raw.clone()).context("decode Q Worker observation batch")?;
    validate_sha256("Q Worker observation digest", &batch.batch_digest)?;
    if let Some(previous) = &batch.previous_digest {
        validate_sha256("Q Worker previous observation digest", previous)?;
    }
    ensure!(
        batch.first_sequence > 0
            && batch.count > 0
            && batch.count <= 128
            && batch.events.len() == usize::try_from(batch.count)?
            && batch.session_observations.len()
                <= batch
                    .events
                    .len()
                    .saturating_mul(MAX_OBSERVATIONS_PER_EVENT),
        "Q Worker observation batch shape is invalid or unbounded"
    );
    let through_sequence = batch
        .first_sequence
        .checked_add(batch.count - 1)
        .context("Q Worker observation sequence overflow")?;
    ensure!(
        through_sequence <= ryeos_state::objects::MAX_HOSTED_SESSION_OBSERVATION_EVENTS,
        "Q Worker observation stream exceeds its cumulative event bound"
    );
    Ok(PreparedBatch {
        first_sequence: batch.first_sequence,
        through_sequence,
        previous_digest: batch.previous_digest,
        batch_digest: batch.batch_digest,
        canonical_json,
    })
}

fn require_turn_contact_for_batch(
    raw: &Value,
    contacted_at_ms: Option<i64>,
    retained_at_ms: i64,
) -> Result<()> {
    let observations = raw
        .get("session_observations")
        .and_then(Value::as_array)
        .context("Q observation batch has no session observation inventory")?;
    if observations
        .iter()
        .any(|observation| observation.get("kind").and_then(Value::as_str) == Some("state"))
    {
        ensure!(
            contacted_at_ms.is_some_and(|contact| retained_at_ms >= contact),
            "Q turn State observation preceded actual turn contact"
        );
    }
    Ok(())
}

fn read_batch_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(
    String,
    String,
    String,
    i64,
    i64,
    Option<String>,
    String,
    String,
    i64,
    i64,
)> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
    ))
}

fn decode_batch_row(
    row: (
        String,
        String,
        String,
        i64,
        i64,
        Option<String>,
        String,
        String,
        i64,
        i64,
    ),
) -> Result<QualificationWorkerObservationBatchRecord> {
    ensure!(
        row.7.len() <= MAX_BATCH_BYTES,
        "retained Q Worker observation exceeds its structural byte bound"
    );
    let batch: Value = serde_json::from_str(&row.7)?;
    let wire_max_bytes = u32::try_from(row.8)?;
    let prepared = prepare_batch(&batch, wire_max_bytes)?;
    ensure!(
        row.7 == prepared.canonical_json
            && u64::try_from(row.3)? == prepared.first_sequence
            && u64::try_from(row.4)? == prepared.through_sequence
            && row.5 == prepared.previous_digest
            && row.6 == prepared.batch_digest
            && row.9 > 0,
        "retained Q Worker observation contradicts its canonical chain"
    );
    validate_sha256("Q Worker process incarnation", &row.1)?;
    validate_sha256("Q Worker request", &row.2)?;
    Ok(QualificationWorkerObservationBatchRecord {
        placement_thread_id: row.0,
        process_incarnation_digest: row.1,
        request_digest: row.2,
        first_sequence: prepared.first_sequence,
        through_sequence: prepared.through_sequence,
        previous_digest: prepared.previous_digest,
        batch_digest: prepared.batch_digest,
        batch,
        wire_max_bytes,
        retained_at_ms: row.9,
    })
}

const BATCH_COLUMNS: &str = "placement_thread_id,process_incarnation_digest,request_digest,
    first_sequence,through_sequence,previous_digest,batch_digest,canonical_batch_json,
    wire_max_bytes,retained_at_ms";

pub(super) fn read_batches(
    conn: &Connection,
    placement: &str,
) -> Result<Vec<QualificationWorkerObservationBatchRecord>> {
    let command = read_command(conn, placement)?;
    let mut statement = conn.prepare(&format!(
        "SELECT {BATCH_COLUMNS} FROM external_qualification_worker_observation_batch
         WHERE placement_thread_id=?1 ORDER BY first_sequence"
    ))?;
    let mut rows = statement.query([placement])?;
    let mut batches = Vec::new();
    let mut previous_digest: Option<String> = None;
    let mut next_sequence = 1_u64;
    let mut total_bytes = 0_i64;
    let mut first_wire_max: Option<u32> = None;
    while let Some(row) = rows.next()? {
        let record = decode_batch_row(read_batch_row(row)?)?;
        let command = command
            .as_ref()
            .context("Q observation has no contacted command")?;
        ensure!(
            command.setup_contacted_at_ms.is_some()
                && record.process_incarnation_digest == command.process_incarnation_digest
                && record.request_digest == command.request_digest
                && record.first_sequence == next_sequence
                && record.previous_digest == previous_digest
                && first_wire_max.is_none_or(|wire| wire == record.wire_max_bytes),
            "Q observation chain changed its command, sequence, or signed wire maximum"
        );
        require_turn_contact_for_batch(
            &record.batch,
            command.contacted_at_ms,
            record.retained_at_ms,
        )?;
        ensure!(
            command
                .setup_final_wire_max_bytes
                .is_none_or(|wire| wire == record.wire_max_bytes),
            "Q observation changed its setup signed wire maximum"
        );
        total_bytes = total_bytes
            .checked_add(i64::try_from(lillux::canonical_json(&record.batch)?.len())?)
            .context("Q observation journal byte count overflow")?;
        ensure!(
            total_bytes <= MAX_JOURNAL_BYTES,
            "Q observation journal exceeds its cumulative byte bound"
        );
        next_sequence = record
            .through_sequence
            .checked_add(1)
            .context("Q observation sequence overflow")?;
        previous_digest = Some(record.batch_digest.clone());
        first_wire_max = Some(record.wire_max_bytes);
        batches.push(record);
    }
    if !batches.is_empty() {
        require_owner(conn, command.as_ref().expect("checked for every row"))?;
    }
    Ok(batches)
}

pub(super) fn validate_batches(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare(
        "SELECT DISTINCT placement_thread_id
         FROM external_qualification_worker_observation_batch ORDER BY placement_thread_id",
    )?;
    let placements = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for placement in placements {
        read_batches(conn, &placement)?;
    }
    Ok(())
}

impl RuntimeDb {
    /// Persist the whole pushed frame before the transport returns its ACK.
    /// The caller authenticates the Worker frame and supplies the selected
    /// signed protocol's maximum. This causal settlement must not reacquire
    /// the Q occurrence gate held by the one-turn sender.
    pub(crate) fn record_qualification_worker_observation_batch(
        &self,
        placement: &str,
        request_digest: &str,
        raw: &Value,
        wire_max_bytes: u32,
    ) -> Result<QualificationWorkerBatchAck> {
        self.conn.with_sql(|conn| {
            validate_sha256("Q Worker request", request_digest)?;
            let batch = prepare_batch(raw, wire_max_bytes)?;
            let first = i64::try_from(batch.first_sequence)?;
            let through = i64::try_from(batch.through_sequence)?;
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let command =
                read_command(&tx, placement)?.context("Q observation has no one-use command")?;
            require_owner(&tx, &command)?;
            let process_phase: String = tx.query_row(
                "SELECT phase FROM external_qualification_worker_process WHERE placement_thread_id=?1",
                [placement],
                |row| row.get(0),
            )?;
            ensure!(
                matches!(
                    command.phase,
                    QualificationWorkerCommandPhase::SetupPossibleContact
                        | QualificationWorkerCommandPhase::SetupSettled
                        | QualificationWorkerCommandPhase::PossibleContact
                        | QualificationWorkerCommandPhase::TurnCompleted
                ) && command.request_digest == request_digest
                    && process_phase == "attached",
                "Q observation has no contacted attached Worker"
            );
            let existing = tx
                .query_row(
                    &format!(
                        "SELECT {BATCH_COLUMNS} FROM external_qualification_worker_observation_batch
                     WHERE placement_thread_id=?1 AND first_sequence=?2"
                    ),
                    params![placement, first],
                    read_batch_row,
                )
                .optional()?;
            if let Some(existing) = existing {
                let existing = decode_batch_row(existing)?;
                require_exact_replay(
                    &existing,
                    &command.process_incarnation_digest,
                    request_digest,
                    &batch,
                    raw,
                    wire_max_bytes,
                )?;
                tx.commit()?;
                return Ok(QualificationWorkerBatchAck {
                    through_sequence: batch.through_sequence,
                    batch_digest: batch.batch_digest,
                });
            }
            let prior = tx
                .query_row(
                    "SELECT through_sequence,batch_digest,wire_max_bytes
                 FROM external_qualification_worker_observation_batch
                 WHERE placement_thread_id=?1 ORDER BY first_sequence DESC LIMIT 1",
                    [placement],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .optional()?;
            require_next_link(
                prior
                    .as_ref()
                    .map(|(through, digest, wire)| (*through, digest.as_str(), *wire)),
                &batch,
                wire_max_bytes,
            )?;
            let prior_bytes: i64 = tx.query_row(
                "SELECT COALESCE(SUM(LENGTH(CAST(canonical_batch_json AS BLOB))),0)
             FROM external_qualification_worker_observation_batch WHERE placement_thread_id=?1",
                [placement],
                |row| row.get(0),
            )?;
            ensure!(
                prior_bytes
                    .checked_add(i64::try_from(batch.canonical_json.len())?)
                    .is_some_and(|total| total <= MAX_JOURNAL_BYTES),
                "Q observation journal exceeds its cumulative byte bound"
            );
            // Clamp only to the durable contact stage: receipt time does not
            // establish causality, and wall-clock regression must not make a
            // legitimate post-turn receipt look like a pre-turn observation.
            let retained_at_ms = lillux::time::timestamp_millis().max(
                command
                    .contacted_at_ms
                    .or(command.setup_contacted_at_ms)
                    .context("Q observation has no session contact boundary")?,
            );
            require_turn_contact_for_batch(raw, command.contacted_at_ms, retained_at_ms)?;
            ensure!(
                command
                    .setup_final_wire_max_bytes
                    .is_none_or(|wire| wire == wire_max_bytes),
                "Q observation changed its setup signed wire maximum"
            );
            ensure!(
                tx.execute(
                    "INSERT INTO external_qualification_worker_observation_batch
                 (placement_thread_id,process_incarnation_digest,request_digest,
                  first_sequence,through_sequence,previous_digest,batch_digest,
                  canonical_batch_json,wire_max_bytes,retained_at_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    params![
                        placement,
                        &command.process_incarnation_digest,
                        request_digest,
                        first,
                        through,
                        &batch.previous_digest,
                        &batch.batch_digest,
                        &batch.canonical_json,
                        wire_max_bytes,
                        retained_at_ms,
                    ],
                )? == 1,
                "Q observation insert did not retain one exact batch"
            );
            tx.commit()?;
            Ok(QualificationWorkerBatchAck {
                through_sequence: batch.through_sequence,
                batch_digest: batch.batch_digest,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn signed_batch(first_sequence: u64, previous_digest: Option<&str>) -> Value {
        let mut value = json!({
            "first_sequence": first_sequence,
            "count": 1,
            "previous_digest": previous_digest,
            "events": [{"event_type": "worker.event", "payload": {}}],
            "session_observations": [],
        });
        let digest = ryeos_state::objects::canonical_value_digest(&value).unwrap();
        value["batch_digest"] = Value::String(digest);
        value
    }

    #[test]
    fn session_setup_batches_allow_bindings_but_fence_turn_state() {
        let session = json!({"session_observations": [
            {"kind": "remote_thread", "remote_thread_id": "session-one"}
        ]});
        require_turn_contact_for_batch(&session, None, 10).unwrap();
        let state = json!({"session_observations": [
            {"kind": "state", "expected": "idle", "next": "turn_running", "turn_id": "one"}
        ]});
        assert!(require_turn_contact_for_batch(&state, None, 10).is_err());
        assert!(require_turn_contact_for_batch(&state, Some(11), 10).is_err());
        require_turn_contact_for_batch(&state, Some(11), 11).unwrap();
    }

    #[test]
    fn q_batch_chain_rejects_gap_digest_swap_and_wire_swap() {
        let first = prepare_batch(&signed_batch(1, None), 4096).unwrap();
        require_next_link(None, &first, 4096).unwrap();
        let next = prepare_batch(&signed_batch(2, Some(&first.batch_digest)), 4096).unwrap();
        require_next_link(Some((1, &first.batch_digest, 4096)), &next, 4096).unwrap();
        assert!(require_next_link(None, &next, 4096).is_err());
        assert!(require_next_link(Some((0, &first.batch_digest, 4096)), &next, 4096).is_err());
        assert!(require_next_link(Some((1, &"0".repeat(64), 4096)), &next, 4096).is_err());
        assert!(require_next_link(Some((1, &first.batch_digest, 2048)), &next, 4096).is_err());
    }

    #[test]
    fn q_batch_replay_requires_exact_process_request_and_content() {
        let raw = signed_batch(1, None);
        let prepared = prepare_batch(&raw, 4096).unwrap();
        let process = "a".repeat(64);
        let request = "b".repeat(64);
        let retained = QualificationWorkerObservationBatchRecord {
            placement_thread_id: "Q-placement".to_owned(),
            process_incarnation_digest: process.clone(),
            request_digest: request.clone(),
            first_sequence: prepared.first_sequence,
            through_sequence: prepared.through_sequence,
            previous_digest: prepared.previous_digest.clone(),
            batch_digest: prepared.batch_digest.clone(),
            batch: raw.clone(),
            wire_max_bytes: 4096,
            retained_at_ms: 1,
        };
        require_exact_replay(&retained, &process, &request, &prepared, &raw, 4096).unwrap();
        assert!(
            require_exact_replay(&retained, &"c".repeat(64), &request, &prepared, &raw, 4096)
                .is_err()
        );
        assert!(
            require_exact_replay(&retained, &process, &"c".repeat(64), &prepared, &raw, 4096)
                .is_err()
        );
        let mut swapped = signed_batch(1, None);
        swapped["events"][0]["payload"] = json!({"different": true});
        let _ = swapped.as_object_mut().unwrap().remove("batch_digest");
        let digest = ryeos_state::objects::canonical_value_digest(&swapped).unwrap();
        swapped["batch_digest"] = Value::String(digest);
        let swapped_prepared = prepare_batch(&swapped, 4096).unwrap();
        assert!(
            require_exact_replay(
                &retained,
                &process,
                &request,
                &swapped_prepared,
                &swapped,
                4096
            )
            .is_err()
        );
        assert!(
            require_exact_replay(&retained, &process, &request, &prepared, &raw, 2048).is_err()
        );
    }

    #[test]
    fn q_batch_rejects_digest_replacement_and_structural_or_signed_overflow() {
        let mut batch = signed_batch(1, None);
        batch["events"][0]["payload"] = json!({"changed": true});
        assert!(prepare_batch(&batch, 4096).is_err());

        let mut batch = signed_batch(1, None);
        batch["count"] = Value::from(2);
        let _ = batch.as_object_mut().unwrap().remove("batch_digest");
        let digest = ryeos_state::objects::canonical_value_digest(&batch).unwrap();
        batch["batch_digest"] = Value::String(digest);
        assert!(prepare_batch(&batch, 4096).is_err());

        let batch = signed_batch(1, None);
        let exact_bytes = lillux::canonical_json(&batch).unwrap().len();
        assert!(prepare_batch(&batch, u32::try_from(exact_bytes - 1).unwrap()).is_err());
        let mut oversized = signed_batch(1, None);
        oversized["events"][0]["payload"] = json!({"body": "x".repeat(MAX_BATCH_BYTES)});
        assert!(prepare_batch(&oversized, MAX_WIRE_BYTES).is_err());
    }
}
