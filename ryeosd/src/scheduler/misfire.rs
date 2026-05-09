//! Misfire policy evaluation.
//!
//! Four policies: skip, fire_once_now, catch_up_bounded:N, catch_up_within_secs:S.
//! Default differs by schedule type (cron→skip, interval→fire_once_now, at→skip).

use anyhow::Result;

use super::db::SchedulerDb;
use super::types::{PendingFire, ScheduleSpecRecord};
use super::crontab::compute_next_fire;

/// All four misfire policies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MisfirePolicy {
    Skip,
    FireOnceNow,
    CatchUpBounded(usize),
    CatchUpWithinSecs(u64),
}

pub fn resolve_misfire_policy(spec: &ScheduleSpecRecord) -> MisfirePolicy {
    if spec.misfire_policy.is_empty() {
        match spec.schedule_type.as_str() {
            "interval" => MisfirePolicy::FireOnceNow,
            _ => MisfirePolicy::Skip,
        }
    } else {
        parse_misfire_policy(&spec.misfire_policy)
    }
}

fn parse_misfire_policy(raw: &str) -> MisfirePolicy {
    match raw {
        "skip" => MisfirePolicy::Skip,
        "fire_once_now" => MisfirePolicy::FireOnceNow,
        s if s.starts_with("catch_up_bounded:") => {
            s.strip_prefix("catch_up_bounded:")
                .and_then(|n| n.parse().ok())
                .map(MisfirePolicy::CatchUpBounded)
                .unwrap_or(MisfirePolicy::Skip)
        }
        s if s.starts_with("catch_up_within_secs:") => {
            s.strip_prefix("catch_up_within_secs:")
                .and_then(|n| n.parse().ok())
                .map(MisfirePolicy::CatchUpWithinSecs)
                .unwrap_or(MisfirePolicy::Skip)
        }
        _ => MisfirePolicy::Skip,
    }
}

/// Detect missed fires between `last_fire_at` and `now`.
/// Returns the list of `scheduled_at` timestamps that have no fire record.
pub fn detect_misfires(
    spec: &ScheduleSpecRecord,
    last_fire_at: i64,
    now: i64,
    db: &SchedulerDb,
) -> Result<Vec<i64>> {
    let mut missed = Vec::new();
    let mut cursor = last_fire_at;

    loop {
        let next = compute_next_fire(
            &spec.schedule_type, &spec.expression, &spec.timezone,
            cursor, Some(cursor),
        )?;

        match next {
            Some(scheduled_at) if scheduled_at <= now => {
                let fid = super::types::fire_id(&spec.schedule_id, scheduled_at);
                if db.get_fire(&fid)?.is_none() {
                    missed.push(scheduled_at);
                }
                cursor = scheduled_at + 1;
            }
            Some(_) => break,
            None => break,
        }

        // Safety: prevent infinite loops for degenerate cases
        if missed.len() > 10_000 {
            tracing::warn!(
                schedule_id = %spec.schedule_id,
                "misfire detection exceeded 10000 fires — truncating"
            );
            break;
        }
    }

    Ok(missed)
}

/// Evaluate the misfire policy for a schedule given detected missed fires.
/// Returns pending fires to dispatch (in chronological order).
/// Skipped fires are recorded to DB inside this function.
pub async fn evaluate_misfires(
    spec: &ScheduleSpecRecord,
    state: &crate::state::AppState,
    now: i64,
) -> Vec<PendingFire> {
    let db = &state.scheduler_db;

    let last_fire_at = match db.get_last_fire(&spec.schedule_id) {
        Ok(Some(f)) => f.scheduled_at,
        Ok(None) => now, // never fired — no gap
        Err(e) => {
            tracing::error!(schedule_id = %spec.schedule_id, error = %e, "failed to get last fire for misfire detection");
            return Vec::new();
        }
    };

    let missed = match detect_misfires(spec, last_fire_at, now, db) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(schedule_id = %spec.schedule_id, error = %e, "misfire detection failed");
            return Vec::new();
        }
    };

    if missed.is_empty() {
        return Vec::new();
    }

    let policy = resolve_misfire_policy(spec);

    match policy {
        MisfirePolicy::Skip => {
            tracing::warn!(
                schedule_id = %spec.schedule_id,
                missed = missed.len(),
                policy = "skip",
                "misfires detected — skipping all"
            );
            for &scheduled_at in &missed {
                let fid = super::types::fire_id(&spec.schedule_id, scheduled_at);
                record_skip(state, &fid, spec, scheduled_at, "misfire_skip").await;
            }
            Vec::new()
        }

        MisfirePolicy::FireOnceNow => {
            tracing::warn!(
                schedule_id = %spec.schedule_id,
                missed = missed.len(),
                policy = "fire_once_now",
                "misfires detected — firing once now"
            );
            for &scheduled_at in &missed[..missed.len().saturating_sub(1)] {
                let fid = super::types::fire_id(&spec.schedule_id, scheduled_at);
                record_skip(state, &fid, spec, scheduled_at, "misfire_skipped_collapse").await;
            }
            vec![PendingFire {
                fire_id: super::types::fire_id(&spec.schedule_id, missed[missed.len() - 1]),
                scheduled_at: missed[missed.len() - 1],
                reason: "misfire_fire_once",
            }]
        }

        MisfirePolicy::CatchUpBounded(max) => {
            // Take most recent `max` fires
            let to_fire: Vec<i64> = missed.iter().rev().take(max).copied().collect();
            let skipped_count = missed.len().saturating_sub(max);
            tracing::warn!(
                schedule_id = %spec.schedule_id,
                missed = missed.len(),
                catching_up = to_fire.len(),
                skipped = skipped_count,
                policy = "catch_up_bounded",
                max,
                "misfires detected — catching up bounded"
            );
            for &scheduled_at in &missed[..skipped_count] {
                let fid = super::types::fire_id(&spec.schedule_id, scheduled_at);
                record_skip(state, &fid, spec, scheduled_at, "misfire_skipped_bounded").await;
            }
            to_fire.into_iter().map(|scheduled_at| PendingFire {
                fire_id: super::types::fire_id(&spec.schedule_id, scheduled_at),
                scheduled_at,
                reason: "misfire_catch_up",
            }).collect()
        }

        MisfirePolicy::CatchUpWithinSecs(window_secs) => {
            let window_ms = window_secs as i64 * 1000;
            let cutoff = now - window_ms;
            let to_fire: Vec<i64> = missed.iter().filter(|&&at| at >= cutoff).copied().collect();
            let skipped_count = missed.len() - to_fire.len();
            tracing::warn!(
                schedule_id = %spec.schedule_id,
                missed = missed.len(),
                catching_up = to_fire.len(),
                skipped = skipped_count,
                policy = "catch_up_within_secs",
                window_secs,
                "misfires detected — catching up within window"
            );
            for &scheduled_at in &missed {
                if scheduled_at < cutoff {
                    let fid = super::types::fire_id(&spec.schedule_id, scheduled_at);
                    record_skip(state, &fid, spec, scheduled_at, "misfire_skipped_window").await;
                }
            }
            to_fire.into_iter().map(|scheduled_at| PendingFire {
                fire_id: super::types::fire_id(&spec.schedule_id, scheduled_at),
                scheduled_at,
                reason: "misfire_catch_up",
            }).collect()
        }
    }
}

async fn record_skip(
    state: &crate::state::AppState,
    fire_id: &str,
    spec: &ScheduleSpecRecord,
    scheduled_at: i64,
    reason: &str,
) {
    let now = lillux::time::timestamp_millis();
    let rec = super::types::FireRecord {
        fire_id: fire_id.to_string(),
        schedule_id: spec.schedule_id.clone(),
        scheduled_at,
        fired_at: Some(now),
        thread_id: None,
        status: "skipped".to_string(),
        trigger_reason: reason.to_string(),
        outcome: None,
        signer_fingerprint: Some(spec.signer_fingerprint.clone()),
    };

    if let Err(e) = state.scheduler_db.upsert_fire(&rec) {
        tracing::error!(fire_id = %fire_id, error = %e, "failed to record skipped fire");
    }

    // Append to JSONL
    let entry = serde_json::json!({
        "entry_type": "skipped",
        "fire_id": fire_id,
        "schedule_id": spec.schedule_id,
        "scheduled_at": scheduled_at,
        "fired_at": now,
        "thread_id": null,
        "skipped_reason": reason,
        "signer_fingerprint": spec.signer_fingerprint,
    });
    append_fire_entry(
        &state.config.system_space_dir,
        &spec.schedule_id,
        &entry,
    );
}

fn append_fire_entry(state_root: &std::path::Path, schedule_id: &str, entry: &serde_json::Value) {
    let fires_dir = state_root.join(".ai").join("state").join("schedules").join(schedule_id);
    let fires_path = fires_dir.join("fires.jsonl");
    if let Err(e) = super::projection::append_jsonl_entry(&fires_path, entry) {
        tracing::error!(schedule_id = %schedule_id, error = %e, "failed to append fire entry");
    }
}
