//! Shared schedule planning helpers.
//!
//! This module centralizes read-only planning computations used by
//! diagnostics and, later, grace-aware dispatch/cursor rebuild. It does
//! not mutate scheduler state and does not make idempotency decisions.

use serde::Serialize;

use super::crontab;
use super::types::{FireRecord, ScheduleSpecRecord};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SchedulePlan {
    pub last_scheduled_at: Option<i64>,
    pub last_fire_at: Option<i64>,
    pub current_due_at: Option<i64>,
    pub current_due_within_grace: Option<bool>,
    pub misfire_horizon_exclusive: Option<i64>,
    pub next_fire_at: Option<i64>,
    pub decision: String,
    pub decision_reason: String,
}

/// Compute a read-only planning snapshot for a schedule at `now_ms`.
pub fn plan_schedule(
    spec: &ScheduleSpecRecord,
    last_fire: Option<&FireRecord>,
    now_ms: i64,
) -> SchedulePlan {
    let last_scheduled_at = last_fire.map(|f| f.scheduled_at);
    let last_fire_at = last_fire.and_then(|f| f.fired_at);
    plan_schedule_from_boundaries(spec, last_scheduled_at, last_fire_at, now_ms)
}

/// Plan from the durable per-schedule cursor without loading a historical fire
/// row. Startup reconciliation uses this after cursor/projection consistency
/// has been checked against the indexed latest fire.
pub fn plan_schedule_from_cursor(
    spec: &ScheduleSpecRecord,
    last_scheduled_at: Option<i64>,
    now_ms: i64,
) -> SchedulePlan {
    plan_schedule_from_boundaries(spec, last_scheduled_at, None, now_ms)
}

fn plan_schedule_from_boundaries(
    spec: &ScheduleSpecRecord,
    last_scheduled_at: Option<i64>,
    last_fire_at: Option<i64>,
    now_ms: i64,
) -> SchedulePlan {
    if !spec.enabled {
        return SchedulePlan {
            last_scheduled_at,
            last_fire_at,
            current_due_at: None,
            current_due_within_grace: None,
            misfire_horizon_exclusive: None,
            next_fire_at: None,
            decision: "disabled".to_string(),
            decision_reason: "schedule is disabled".to_string(),
        };
    }

    let current_due_at = compute_current_due_at(spec, last_scheduled_at, now_ms);
    let next_fire_at = compute_next_planned_fire(spec, last_scheduled_at, now_ms, current_due_at);
    let current_due_within_grace =
        current_due_at.map(|due| within_lateness_grace(spec, due, now_ms));
    let misfire_horizon_exclusive = current_due_at.map(|due| {
        if within_lateness_grace(spec, due, now_ms) {
            due
        } else {
            now_ms
        }
    });

    let (decision, decision_reason) = match (current_due_at, current_due_within_grace, next_fire_at)
    {
        (Some(_), Some(true), _) => (
            "due_within_grace".to_string(),
            "current due boundary is within lateness grace".to_string(),
        ),
        (Some(_), Some(false), _) => (
            "late_misfire_evaluation".to_string(),
            "current due boundary is beyond lateness grace".to_string(),
        ),
        (None, _, Some(next)) if next > now_ms => (
            "waiting_for_next_boundary".to_string(),
            "next fire is in the future".to_string(),
        ),
        (None, _, None) if spec.schedule_type == "at" => (
            "exhausted".to_string(),
            "one-shot schedule has no future fire".to_string(),
        ),
        (None, _, None) => (
            "invalid_or_exhausted".to_string(),
            "schedule expression produced no current or future fire".to_string(),
        ),
        _ => (
            "waiting_for_next_boundary".to_string(),
            "next fire is in the future".to_string(),
        ),
    };

    SchedulePlan {
        last_scheduled_at,
        last_fire_at,
        current_due_at,
        current_due_within_grace,
        misfire_horizon_exclusive,
        next_fire_at,
        decision,
        decision_reason,
    }
}

fn compute_current_due_at(
    spec: &ScheduleSpecRecord,
    last_scheduled_at: Option<i64>,
    now_ms: i64,
) -> Option<i64> {
    if spec.schedule_type == "at" {
        return crontab::parse_iso_ts(&spec.expression).and_then(|target| {
            let already_recorded = last_scheduled_at.is_some_and(|last| last >= target);
            if !already_recorded && target <= now_ms {
                Some(target)
            } else {
                None
            }
        });
    }

    if spec.schedule_type == "interval" {
        return compute_current_interval_due(spec, last_scheduled_at, now_ms);
    }

    match crontab::compute_scheduled_at(
        &spec.schedule_type,
        &spec.expression,
        &spec.timezone,
        now_ms,
        last_scheduled_at,
        Some(spec.registered_at),
    ) {
        Some(at) if at <= now_ms => Some(at),
        Some(_) => None,
        None => None,
    }
}

fn compute_next_planned_fire(
    spec: &ScheduleSpecRecord,
    last_scheduled_at: Option<i64>,
    now_ms: i64,
    current_due_at: Option<i64>,
) -> Option<i64> {
    match spec.schedule_type.as_str() {
        "interval" => compute_next_interval_fire(spec, last_scheduled_at, now_ms, current_due_at),
        "cron" => crontab::compute_next_fire(
            &spec.schedule_type,
            &spec.expression,
            &spec.timezone,
            now_ms,
            None,
        )
        .ok()
        .flatten(),
        "at" => crontab::parse_iso_ts(&spec.expression).and_then(|target| {
            let already_recorded = last_scheduled_at.is_some_and(|last| last >= target);
            if !already_recorded && target > now_ms {
                Some(target)
            } else {
                None
            }
        }),
        _ => None,
    }
}

fn compute_next_interval_fire(
    spec: &ScheduleSpecRecord,
    last_scheduled_at: Option<i64>,
    now_ms: i64,
    current_due_at: Option<i64>,
) -> Option<i64> {
    let interval_ms = interval_ms(spec)?;
    let first_fire = spec.registered_at + interval_ms;

    if last_scheduled_at.is_none() && first_fire > now_ms {
        return Some(first_fire);
    }

    let anchor = current_due_at.or(last_scheduled_at).unwrap_or(first_fire);
    let mut next = anchor + interval_ms;
    if next <= now_ms {
        let elapsed = now_ms - anchor;
        let intervals_elapsed = elapsed / interval_ms;
        next = anchor + (intervals_elapsed + 1) * interval_ms;
    }
    Some(next)
}

fn compute_current_interval_due(
    spec: &ScheduleSpecRecord,
    last_scheduled_at: Option<i64>,
    now_ms: i64,
) -> Option<i64> {
    let interval_ms = interval_ms(spec)?;
    let anchor = last_scheduled_at.unwrap_or(spec.registered_at);
    let first_due = anchor + interval_ms;
    if first_due > now_ms {
        return None;
    }
    let elapsed = now_ms - first_due;
    let intervals_elapsed = elapsed / interval_ms;
    Some(first_due + intervals_elapsed * interval_ms)
}

fn interval_ms(spec: &ScheduleSpecRecord) -> Option<i64> {
    let secs = spec.expression.parse::<i64>().ok()?;
    if secs <= 0 {
        return None;
    }
    Some(secs * 1000)
}

fn within_lateness_grace(spec: &ScheduleSpecRecord, scheduled_at: i64, now_ms: i64) -> bool {
    let grace_ms = spec.lateness_grace_secs.saturating_mul(1000);
    now_ms <= scheduled_at.saturating_add(grace_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_spec(schedule_type: &str, expression: &str, registered_at: i64) -> ScheduleSpecRecord {
        ScheduleSpecRecord {
            schedule_id: "test".to_string(),
            item_ref: "directive:test".to_string(),
            ref_bindings: std::collections::BTreeMap::new(),
            params: "{}".to_string(),
            schedule_type: schedule_type.to_string(),
            expression: expression.to_string(),
            timezone: "UTC".to_string(),
            misfire_policy: "skip".to_string(),
            overlap_policy: "skip".to_string(),
            lateness_grace_secs: 60,
            enabled: true,
            project_root: None,
            signer_fingerprint: "11".repeat(32),
            spec_hash: "22".repeat(32),
            registered_at,
            requester_fingerprint: "fp:test".to_string(),
            capabilities: vec!["ryeos.execute.*".to_string()],
        }
    }

    fn make_fire(scheduled_at: i64) -> FireRecord {
        let fire_id = format!("test@{scheduled_at}");
        FireRecord {
            thread_id: Some(super::super::types::thread_id_from_fire(&fire_id)),
            fire_id,
            schedule_id: "test".to_string(),
            scheduled_at,
            fired_at: Some(scheduled_at + 1),
            completed_at: Some(scheduled_at + 1),
            status: "completed".to_string(),
            trigger_reason: "normal".to_string(),
            outcome: Some("success".to_string()),
            signer_fingerprint: "11".repeat(32),
        }
    }

    #[test]
    fn interval_first_fire_uses_registered_anchor() {
        let spec = make_spec("interval", "60", 1_000);
        let plan = plan_schedule(&spec, None, 30_000);
        assert_eq!(plan.current_due_at, None);
        assert_eq!(plan.next_fire_at, Some(61_000));
        assert_eq!(plan.decision, "waiting_for_next_boundary");
    }

    #[test]
    fn interval_due_after_registered_anchor() {
        let spec = make_spec("interval", "60", 1_000);
        let plan = plan_schedule(&spec, None, 182_000);
        assert_eq!(plan.current_due_at, Some(181_000));
        assert_eq!(plan.current_due_within_grace, Some(true));
        assert_eq!(plan.misfire_horizon_exclusive, Some(181_000));
        assert_eq!(plan.decision, "due_within_grace");
    }

    #[test]
    fn interval_due_beyond_grace_uses_misfire_horizon_now() {
        let mut spec = make_spec("interval", "60", 1_000);
        spec.lateness_grace_secs = 1;
        let plan = plan_schedule(&spec, None, 183_000);
        assert_eq!(plan.current_due_at, Some(181_000));
        assert_eq!(plan.current_due_within_grace, Some(false));
        assert_eq!(plan.misfire_horizon_exclusive, Some(183_000));
        assert_eq!(plan.decision, "late_misfire_evaluation");
    }

    #[test]
    fn grace_boundary_is_inclusive_until_next_millisecond() {
        let mut spec = make_spec("interval", "60", 1_000);
        spec.lateness_grace_secs = 1;

        let boundary = plan_schedule(&spec, None, 182_000);
        assert_eq!(boundary.current_due_at, Some(181_000));
        assert_eq!(boundary.current_due_within_grace, Some(true));
        assert_eq!(boundary.misfire_horizon_exclusive, Some(181_000));

        let after_boundary = plan_schedule(&spec, None, 182_001);
        assert_eq!(after_boundary.current_due_at, Some(181_000));
        assert_eq!(after_boundary.current_due_within_grace, Some(false));
        assert_eq!(after_boundary.misfire_horizon_exclusive, Some(182_001));
    }

    #[test]
    fn disabled_schedule_has_no_next_fire() {
        let mut spec = make_spec("interval", "60", 1_000);
        spec.enabled = false;
        let plan = plan_schedule(&spec, None, 62_000);
        assert_eq!(plan.current_due_at, None);
        assert_eq!(plan.next_fire_at, None);
        assert_eq!(plan.decision, "disabled");
    }

    #[test]
    fn at_future_schedule_waits() {
        let spec = make_spec("at", "2028-01-01T00:00:00Z", 1_000);
        let target = crontab::parse_iso_ts("2028-01-01T00:00:00Z").unwrap();
        let plan = plan_schedule(&spec, None, target - 1_000);
        assert_eq!(plan.current_due_at, None);
        assert_eq!(plan.next_fire_at, Some(target));
        assert_eq!(plan.decision, "waiting_for_next_boundary");
    }

    #[test]
    fn at_recorded_schedule_is_exhausted() {
        let spec = make_spec("at", "2028-01-01T00:00:00Z", 1_000);
        let target = crontab::parse_iso_ts("2028-01-01T00:00:00Z").unwrap();
        let fire = make_fire(target);
        let plan = plan_schedule(&spec, Some(&fire), target + 1_000);
        assert_eq!(plan.current_due_at, None);
        assert_eq!(plan.next_fire_at, None);
        assert_eq!(plan.decision, "exhausted");
    }
}
