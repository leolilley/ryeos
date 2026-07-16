//! Misfire policy evaluation.
//!
//! Four policies: skip, fire_once_now, catch_up_bounded:N, catch_up_within_secs:S.
//! Every schedule must author one explicitly; invalid or absent policy fails
//! closed instead of selecting behavior in Rust.

use anyhow::{Context, Result};

use super::crontab::compute_next_fire;
use super::db::SchedulerDb;
use super::projection;
use super::types::{FireRecord, PendingFire, ScheduleSpecRecord};
use crate::SchedulerContext;

// Processing bound only, not schedule policy: subsequent timer passes resume
// from the persisted fire/cursor boundary and continue the same downtime gap.
const MISFIRE_SCAN_BATCH: usize = 10_000;

/// All four misfire policies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MisfirePolicy {
    Skip,
    FireOnceNow,
    CatchUpBounded(usize),
    CatchUpWithinSecs(u64),
}

pub fn resolve_misfire_policy(spec: &ScheduleSpecRecord) -> Result<MisfirePolicy> {
    parse_misfire_policy(&spec.misfire_policy)
}

pub fn parse_misfire_policy(raw: &str) -> Result<MisfirePolicy> {
    match raw {
        "skip" => Ok(MisfirePolicy::Skip),
        "fire_once_now" => Ok(MisfirePolicy::FireOnceNow),
        s if s.starts_with("catch_up_bounded:") => {
            let count = s
                .strip_prefix("catch_up_bounded:")
                .expect("prefix checked")
                .parse::<usize>()
                .map_err(|_| anyhow::anyhow!("invalid misfire_policy: {raw}"))?;
            Ok(MisfirePolicy::CatchUpBounded(count))
        }
        s if s.starts_with("catch_up_within_secs:") => {
            let seconds = s
                .strip_prefix("catch_up_within_secs:")
                .expect("prefix checked")
                .parse::<u64>()
                .map_err(|_| anyhow::anyhow!("invalid misfire_policy: {raw}"))?;
            if seconds > (i64::MAX as u64) / 1_000 {
                anyhow::bail!("misfire_policy window is too large: {raw}");
            }
            Ok(MisfirePolicy::CatchUpWithinSecs(seconds))
        }
        _ => anyhow::bail!("invalid misfire_policy: {raw}"),
    }
}

/// Detect missed fires after `anchor` and before `horizon_exclusive`.
///
/// The upper bound is intentionally exclusive. The live timer evaluates
/// misfires immediately before dispatching the current due fire, and small
/// post-boundary timer latency must not convert that current fire into a
/// misfire. Startup reconciliation can pass `now` as the horizon to evaluate
/// genuinely missed fires from daemon downtime.
pub fn detect_misfires_before(
    spec: &ScheduleSpecRecord,
    anchor: i64,
    horizon_exclusive: i64,
    db: &SchedulerDb,
) -> Result<Vec<i64>> {
    let mut missed = Vec::new();
    let mut cursor = anchor;
    let mut scanned = 0usize;

    if cursor >= horizon_exclusive {
        return Ok(missed);
    }

    loop {
        let next = compute_next_fire(
            &spec.schedule_type,
            &spec.expression,
            &spec.timezone,
            cursor,
            Some(cursor),
        )?;

        match next {
            Some(scheduled_at) if scheduled_at < horizon_exclusive => {
                scanned += 1;
                let fid = super::types::fire_id(&spec.schedule_id, scheduled_at);
                // The cursor normally places us strictly after the latest
                // indexed fire. Point lookups preserve correctness if a cursor
                // was stale after a prior best-effort refresh, without loading
                // the schedule's complete historical fire-id set at startup.
                match db.get_fire(&fid)? {
                    Some(existing) => existing
                        .validate()
                        .with_context(|| format!("invalid indexed scheduler fire {fid}"))?,
                    None => missed.push(scheduled_at),
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
        if scanned >= MISFIRE_SCAN_BATCH {
            tracing::warn!(
                schedule_id = %spec.schedule_id,
                scanned,
                "misfire detection reached its bounded scan batch"
            );
            break;
        }
    }

    Ok(missed)
}

/// Evaluate the misfire policy for fires before `horizon_exclusive`.
/// Returns pending fires to dispatch (in chronological order).
/// Skipped fires are recorded to DB inside this function.
pub async fn evaluate_misfires_before<Ctx: SchedulerContext>(
    spec: &ScheduleSpecRecord,
    ctx: &Ctx,
    horizon_exclusive: i64,
    evaluation_now: i64,
) -> Result<Vec<PendingFire>> {
    let db = ctx.scheduler_db();

    let anchor = match db
        .get_cursor(&spec.schedule_id)
        .with_context(|| format!("load scheduler cursor for {}", spec.schedule_id))?
    {
        Some(cursor) if cursor.spec_hash == spec.spec_hash => cursor
            .last_scheduled_at
            .unwrap_or(spec.registered_at)
            .max(spec.registered_at),
        _ => match db
            .get_last_fire(&spec.schedule_id)
            .with_context(|| format!("load indexed last fire for {}", spec.schedule_id))?
        {
            Some(fire) => {
                fire.validate().with_context(|| {
                    format!("invalid indexed last fire for {}", spec.schedule_id)
                })?;
                fire.scheduled_at.max(spec.registered_at)
            }
            None => spec.registered_at,
        },
    };

    if anchor >= horizon_exclusive {
        return Ok(Vec::new());
    }

    let missed = detect_misfires_before(spec, anchor, horizon_exclusive, &db)
        .with_context(|| format!("detect misfires for {}", spec.schedule_id))?;

    if missed.is_empty() {
        return Ok(Vec::new());
    }

    let policy = resolve_misfire_policy(spec).with_context(|| {
        format!(
            "invalid authored misfire policy for schedule {}",
            spec.schedule_id
        )
    })?;

    let pending = match policy {
        MisfirePolicy::Skip => {
            tracing::warn!(
                schedule_id = %spec.schedule_id,
                missed = missed.len(),
                policy = "skip",
                "misfires detected — skipping all"
            );
            for &scheduled_at in &missed {
                let fid = super::types::fire_id(&spec.schedule_id, scheduled_at);
                record_skip(ctx, &fid, spec, scheduled_at, "misfire_skip").await?;
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
                record_skip(ctx, &fid, spec, scheduled_at, "misfire_skipped_collapse").await?;
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
                record_skip(ctx, &fid, spec, scheduled_at, "misfire_skipped_bounded").await?;
            }
            to_fire
                .into_iter()
                .map(|scheduled_at| PendingFire {
                    fire_id: super::types::fire_id(&spec.schedule_id, scheduled_at),
                    scheduled_at,
                    reason: "misfire_catch_up",
                })
                .collect()
        }

        MisfirePolicy::CatchUpWithinSecs(window_secs) => {
            let window_ms =
                i64::try_from(window_secs).expect("validated misfire window fits i64") * 1000;
            let cutoff = evaluation_now - window_ms;
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
                    record_skip(ctx, &fid, spec, scheduled_at, "misfire_skipped_window").await?;
                }
            }
            to_fire
                .into_iter()
                .map(|scheduled_at| PendingFire {
                    fire_id: super::types::fire_id(&spec.schedule_id, scheduled_at),
                    scheduled_at,
                    reason: "misfire_catch_up",
                })
                .collect()
        }
    };
    Ok(pending)
}

async fn record_skip<Ctx: SchedulerContext>(
    ctx: &Ctx,
    fire_id: &str,
    spec: &ScheduleSpecRecord,
    scheduled_at: i64,
    reason: &str,
) -> Result<()> {
    let now = lillux::time::timestamp_millis();
    let rec = FireRecord {
        fire_id: fire_id.to_string(),
        schedule_id: spec.schedule_id.clone(),
        scheduled_at,
        fired_at: Some(now),
        completed_at: Some(now),
        thread_id: None,
        status: "skipped".to_string(),
        trigger_reason: reason.to_string(),
        outcome: Some(reason.to_string()),
        signer_fingerprint: spec.signer_fingerprint.clone(),
    };

    let app_root = ctx.app_root().to_path_buf();

    {
        let db = ctx.scheduler_db();
        let fire_id_owned = fire_id.to_string();
        tokio::task::spawn_blocking(move || {
            projection::persist_fire_snapshot(&app_root, &db, &rec)
                .with_context(|| format!("persist skipped misfire {fire_id_owned}"))
        })
        .await
        .context("misfire skip persistence task stopped")??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_spec(misfire_policy: &str, schedule_type: &str) -> ScheduleSpecRecord {
        ScheduleSpecRecord {
            schedule_id: "test".to_string(),
            item_ref: "directive:test".to_string(),
            ref_bindings: std::collections::BTreeMap::new(),
            params: "{}".to_string(),
            schedule_type: schedule_type.to_string(),
            expression: "60".to_string(),
            timezone: "UTC".to_string(),
            misfire_policy: misfire_policy.to_string(),
            overlap_policy: "skip".to_string(),
            lateness_grace_secs: 60,
            enabled: true,
            project_root: None,
            signer_fingerprint: "11".repeat(32),
            spec_hash: "22".repeat(32),
            registered_at: 0,
            requester_fingerprint: "fp:test".to_string(),
            capabilities: vec!["ryeos.execute.*".to_string()],
        }
    }

    fn make_cron_spec(registered_at: i64) -> ScheduleSpecRecord {
        ScheduleSpecRecord {
            expression: "0 */15 * * * *".to_string(),
            registered_at,
            ..make_spec("skip", "cron")
        }
    }

    fn make_fire(schedule_id: &str, scheduled_at: i64) -> FireRecord {
        let fire_id = super::super::types::fire_id(schedule_id, scheduled_at);
        FireRecord {
            thread_id: Some(super::super::types::thread_id_from_fire(&fire_id)),
            fire_id,
            schedule_id: schedule_id.to_string(),
            scheduled_at,
            fired_at: Some(scheduled_at),
            completed_at: Some(scheduled_at + 1),
            status: "completed".to_string(),
            trigger_reason: "normal".to_string(),
            outcome: Some("success".to_string()),
            signer_fingerprint: "11".repeat(32),
        }
    }

    #[test]
    fn resolve_skip() {
        let spec = make_spec("skip", "cron");
        assert_eq!(resolve_misfire_policy(&spec).unwrap(), MisfirePolicy::Skip);
    }

    #[test]
    fn resolve_fire_once_now() {
        let spec = make_spec("fire_once_now", "cron");
        assert_eq!(
            resolve_misfire_policy(&spec).unwrap(),
            MisfirePolicy::FireOnceNow
        );
    }

    #[test]
    fn resolve_catch_up_bounded() {
        let spec = make_spec("catch_up_bounded:5", "cron");
        assert_eq!(
            resolve_misfire_policy(&spec).unwrap(),
            MisfirePolicy::CatchUpBounded(5)
        );
    }

    #[test]
    fn resolve_catch_up_within_secs() {
        let spec = make_spec("catch_up_within_secs:300", "cron");
        assert_eq!(
            resolve_misfire_policy(&spec).unwrap(),
            MisfirePolicy::CatchUpWithinSecs(300)
        );
    }

    #[test]
    fn empty_policy_is_rejected_for_cron() {
        let spec = make_spec("", "cron");
        assert!(resolve_misfire_policy(&spec).is_err());
    }

    #[test]
    fn empty_policy_is_rejected_for_interval() {
        let spec = make_spec("", "interval");
        assert!(resolve_misfire_policy(&spec).is_err());
    }

    #[test]
    fn empty_policy_is_rejected_for_at() {
        let spec = make_spec("", "at");
        assert!(resolve_misfire_policy(&spec).is_err());
    }

    #[test]
    fn invalid_policy_is_rejected() {
        let spec = make_spec("totally_invalid", "cron");
        assert!(resolve_misfire_policy(&spec).is_err());
    }

    #[test]
    fn invalid_catch_up_bound_is_rejected() {
        let spec = make_spec("catch_up_bounded:not_a_number", "cron");
        assert!(resolve_misfire_policy(&spec).is_err());
    }

    #[test]
    fn detect_misfires_excludes_current_due_boundary() {
        let db = SchedulerDb::new_in_memory().unwrap();
        let spec = make_cron_spec(1780455600000); // 2026-06-03T03:00:00Z
        db.upsert_fire(&make_fire(&spec.schedule_id, 1780457400000))
            .unwrap(); // 03:30

        let missed = detect_misfires_before(
            &spec,
            1780457400000, // 03:30
            1780458300000, // 03:45, current due fire; exclusive
            &db,
        )
        .unwrap();

        assert!(
            missed.is_empty(),
            "current due fire must be dispatched normally, not treated as a misfire"
        );
    }

    #[test]
    fn detect_misfires_includes_older_missing_boundary_before_current() {
        let db = SchedulerDb::new_in_memory().unwrap();
        let spec = make_cron_spec(1780455600000); // 2026-06-03T03:00:00Z
        db.upsert_fire(&make_fire(&spec.schedule_id, 1780456500000))
            .unwrap(); // 03:15

        let missed = detect_misfires_before(
            &spec,
            1780456500000, // 03:15
            1780458300000, // 03:45, current due fire; exclusive
            &db,
        )
        .unwrap();

        assert_eq!(missed, vec![1780457400000]); // 03:30
    }

    #[test]
    fn detect_misfires_startup_after_missed_first_fire() {
        let db = SchedulerDb::new_in_memory().unwrap();
        let spec = make_cron_spec(1780457760000); // 2026-06-03T03:36:00Z

        let missed = detect_misfires_before(
            &spec,
            spec.registered_at,
            1780458720000, // 03:52; startup reconcile horizon
            &db,
        )
        .unwrap();

        assert_eq!(missed, vec![1780458300000]); // 03:45
    }

    #[test]
    fn detect_misfires_startup_before_first_fire_is_empty() {
        let db = SchedulerDb::new_in_memory().unwrap();
        let spec = make_cron_spec(1780457760000); // 2026-06-03T03:36:00Z

        let missed = detect_misfires_before(
            &spec,
            spec.registered_at,
            1780458000000, // 03:40; before first 03:45 fire
            &db,
        )
        .unwrap();

        assert!(missed.is_empty());
    }
}
