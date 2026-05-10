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

    // Batch-load all existing fire IDs for this schedule upfront to avoid
    // N+1 individual get_fire() queries inside the loop.
    let existing_ids = db.get_existing_fire_ids(&spec.schedule_id)?;

    loop {
        let next = compute_next_fire(
            &spec.schedule_type, &spec.expression, &spec.timezone,
            cursor, Some(cursor),
        )?;

        match next {
            Some(scheduled_at) if scheduled_at <= now => {
                let fid = super::types::fire_id(&spec.schedule_id, scheduled_at);
                if !existing_ids.contains(&fid) {
                    missed.push(scheduled_at);
                }
                // Use the scheduled_at as the new cursor (not +1) to avoid
                // drifting interval calculations. compute_next_fire handles
                // finding the next fire from this anchor.
                cursor = scheduled_at;
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
            // Take oldest `max` fires (already in chronological order)
            let to_fire: Vec<i64> = missed.iter().take(max).copied().collect();
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
            // Skip the fires beyond the bound (newest ones)
            for &scheduled_at in &missed[max..] {
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
        completed_at: None,
        thread_id: None,
        status: "skipped".to_string(),
        trigger_reason: reason.to_string(),
        outcome: None,
        signer_fingerprint: Some(spec.signer_fingerprint.clone()),
    };

    let entry = serde_json::json!({
        "entry_type": "skipped",
        "status": "skipped",
        "fire_id": fire_id,
        "schedule_id": spec.schedule_id,
        "scheduled_at": scheduled_at,
        "fired_at": now,
        "thread_id": null,
        "skipped_reason": reason,
        "signer_fingerprint": spec.signer_fingerprint,
    });
    let fires_path = state.config.system_space_dir
        .join(ryeos_engine::AI_DIR).join("state").join("schedules")
        .join(&spec.schedule_id).join("fires.jsonl");

    {
        let db = state.scheduler_db.clone();
        let fire_id_owned = fire_id.to_string();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = db.upsert_fire(&rec) {
                tracing::error!(fire_id = %fire_id_owned, error = %e, "failed to record skipped fire");
            }
            if let Err(e) = super::projection::append_jsonl_entry(&fires_path, &entry) {
                tracing::error!(fire_id = %fire_id_owned, error = %e, "failed to append fire entry");
            }
        }).await.unwrap_or_else(|e| {
            tracing::error!(fire_id = %fire_id, error = %e, "spawn_blocking task panicked or was cancelled (misfire skip persist)");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_spec(misfire_policy: &str, schedule_type: &str) -> ScheduleSpecRecord {
        ScheduleSpecRecord {
            schedule_id: "test".to_string(),
            item_ref: "directive:test".to_string(),
            params: "{}".to_string(),
            schedule_type: schedule_type.to_string(),
            expression: "60".to_string(),
            timezone: "UTC".to_string(),
            misfire_policy: misfire_policy.to_string(),
            overlap_policy: "skip".to_string(),
            enabled: true,
            project_root: None,
            signer_fingerprint: "fp:test".to_string(),
            spec_hash: "abc".to_string(),
            last_modified: 0,
            requester_fingerprint: "fp:test".to_string(),
            capabilities: vec!["ryeos.execute.*".to_string()],
        }
    }

    #[test]
    fn resolve_skip() {
        let spec = make_spec("skip", "cron");
        assert_eq!(resolve_misfire_policy(&spec), MisfirePolicy::Skip);
    }

    #[test]
    fn resolve_fire_once_now() {
        let spec = make_spec("fire_once_now", "cron");
        assert_eq!(resolve_misfire_policy(&spec), MisfirePolicy::FireOnceNow);
    }

    #[test]
    fn resolve_catch_up_bounded() {
        let spec = make_spec("catch_up_bounded:5", "cron");
        assert_eq!(resolve_misfire_policy(&spec), MisfirePolicy::CatchUpBounded(5));
    }

    #[test]
    fn resolve_catch_up_within_secs() {
        let spec = make_spec("catch_up_within_secs:300", "cron");
        assert_eq!(resolve_misfire_policy(&spec), MisfirePolicy::CatchUpWithinSecs(300));
    }

    #[test]
    fn resolve_default_cron_is_skip() {
        let spec = make_spec("", "cron");
        assert_eq!(resolve_misfire_policy(&spec), MisfirePolicy::Skip);
    }

    #[test]
    fn resolve_default_interval_is_fire_once_now() {
        let spec = make_spec("", "interval");
        assert_eq!(resolve_misfire_policy(&spec), MisfirePolicy::FireOnceNow);
    }

    #[test]
    fn resolve_default_at_is_skip() {
        let spec = make_spec("", "at");
        assert_eq!(resolve_misfire_policy(&spec), MisfirePolicy::Skip);
    }

    #[test]
    fn resolve_invalid_policy_defaults_to_skip() {
        let spec = make_spec("totally_invalid", "cron");
        assert_eq!(resolve_misfire_policy(&spec), MisfirePolicy::Skip);
    }

    #[test]
    fn resolve_catch_up_bounded_invalid_number_defaults_to_skip() {
        let spec = make_spec("catch_up_bounded:not_a_number", "cron");
        assert_eq!(resolve_misfire_policy(&spec), MisfirePolicy::Skip);
    }
}
