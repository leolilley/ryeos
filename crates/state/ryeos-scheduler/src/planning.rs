//! Shared schedule planning helpers.
//!
//! This module centralizes read-only planning computations used by
//! diagnostics and, later, grace-aware dispatch/cursor rebuild. It does
//! not mutate scheduler state and does not make idempotency decisions.

use ryeos_scheduler_contract::{ScheduleAutoPauseReason, parse_iso_ts};
use serde::Serialize;

use super::crontab;
use super::types::{FireRecord, ScheduleSpecRecord};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SchedulePlan {
    pub last_scheduled_at: Option<i64>,
    pub last_dispatched_at: Option<i64>,
    pub current_due_at: Option<i64>,
    pub current_due_within_grace: Option<bool>,
    pub misfire_horizon_exclusive: Option<i64>,
    pub next_fire_at: Option<i64>,
    pub decision: String,
    pub decision_reason: String,
    /// Set when the schedule's signed lifetime bounds admit no further
    /// boundary: the timer pauses the schedule with this reason.
    pub lifetime_exhausted: Option<ScheduleAutoPauseReason>,
}

/// The last boundary a schedule's lifetime bounds admit (inclusive), and the
/// bound that set it. `None` when the schedule declares no lifetime bound, or
/// when `max_fires` names a boundary its expression never reaches and no
/// `expires_at` is set.
///
/// `max_fires` counts boundaries from registration, exactly as the planner
/// walks them: the first interval boundary is `registered_at + interval`,
/// cron boundaries are strictly after `registered_at`, and a one-shot `at`
/// schedule has one boundary. Each counts whether it ran or was skipped, so
/// the bound does not depend on retained fire history.
pub fn lifetime_horizon(spec: &ScheduleSpecRecord) -> Option<(i64, ScheduleAutoPauseReason)> {
    let by_count = spec
        .execution
        .max_fires
        .and_then(|max_fires| nth_boundary(spec, max_fires))
        .map(|last| (last, ScheduleAutoPauseReason::MaxFiresReached));
    let by_expiry = spec.execution.expires_at_ms().map(|expires_at| {
        (
            expires_at.saturating_sub(1),
            ScheduleAutoPauseReason::Expired,
        )
    });
    match (by_count, by_expiry) {
        (Some(count), Some(expiry)) => Some(if count.0 <= expiry.0 { count } else { expiry }),
        (count, expiry) => count.or(expiry),
    }
}

fn nth_boundary(spec: &ScheduleSpecRecord, n: u32) -> Option<i64> {
    if n == 0 {
        return None;
    }
    match spec.schedule_type.as_str() {
        "interval" => interval_ms(spec)?
            .checked_mul(i64::from(n))?
            .checked_add(spec.registered_at),
        "cron" => crontab::nth_cron_boundary_after(
            &spec.expression,
            &spec.timezone,
            spec.registered_at,
            n,
        ),
        "at" => parse_iso_ts(&spec.expression),
        _ => None,
    }
}

/// Clamp an exclusive misfire horizon to the schedule's lifetime bounds, so
/// misfire evaluation never plans a boundary the lifetime does not admit.
pub fn clamp_to_lifetime_exclusive(spec: &ScheduleSpecRecord, horizon_exclusive: i64) -> i64 {
    match lifetime_horizon(spec) {
        Some((last, _)) => horizon_exclusive.min(last.saturating_add(1)),
        None => horizon_exclusive,
    }
}

/// Compute a read-only planning snapshot for a schedule at `now_ms`.
pub fn plan_schedule(
    spec: &ScheduleSpecRecord,
    last_fire: Option<&FireRecord>,
    now_ms: i64,
) -> SchedulePlan {
    let last_scheduled_at = last_fire.map(|f| f.scheduled_at);
    let last_dispatched_at = last_fire.and_then(|f| f.dispatched_at);
    plan_schedule_from_boundaries(spec, last_scheduled_at, last_dispatched_at, now_ms)
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
    last_dispatched_at: Option<i64>,
    now_ms: i64,
) -> SchedulePlan {
    if !spec.enabled {
        return SchedulePlan {
            last_scheduled_at,
            last_dispatched_at,
            current_due_at: None,
            current_due_within_grace: None,
            misfire_horizon_exclusive: None,
            next_fire_at: None,
            decision: "disabled".to_string(),
            decision_reason: "schedule is disabled".to_string(),
            lifetime_exhausted: None,
        };
    }

    // Lifetime bounds cap which boundaries exist at all. Planning as of
    // `min(now, last admitted boundary)` makes the last admitted boundary the
    // current one even when the daemon was down past it, so its misfire
    // policy still decides it; nothing later is planned.
    let horizon = lifetime_horizon(spec);
    let admitted_now = horizon.map_or(now_ms, |(last, _)| now_ms.min(last));
    let current_due_at = compute_current_due_at(spec, last_scheduled_at, admitted_now);
    let next_fire_at = compute_next_planned_fire(spec, last_scheduled_at, now_ms, current_due_at)
        .filter(|next| horizon.is_none_or(|(last, _)| *next <= last));
    let current_due_within_grace =
        current_due_at.map(|due| within_lateness_grace(spec, due, now_ms));
    let misfire_horizon_exclusive = current_due_at.map(|due| {
        if within_lateness_grace(spec, due, now_ms) {
            due
        } else {
            clamp_to_lifetime_exclusive(spec, now_ms)
        }
    });
    let lifetime_exhausted = horizon
        .filter(|_| current_due_at.is_none() && next_fire_at.is_none())
        .map(|(_, reason)| reason);

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
        (None, _, None) if lifetime_exhausted.is_some() => (
            "lifetime_exhausted".to_string(),
            format!(
                "schedule lifetime bound reached: {}",
                lifetime_exhausted.expect("checked by the guard")
            ),
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
        last_dispatched_at,
        current_due_at,
        current_due_within_grace,
        misfire_horizon_exclusive,
        next_fire_at,
        decision,
        decision_reason,
        lifetime_exhausted,
    }
}

fn compute_current_due_at(
    spec: &ScheduleSpecRecord,
    last_scheduled_at: Option<i64>,
    now_ms: i64,
) -> Option<i64> {
    if spec.schedule_type == "at" {
        return parse_iso_ts(&spec.expression).and_then(|target| {
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
        "at" => parse_iso_ts(&spec.expression).and_then(|target| {
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

    fn projectless_execution() -> ryeos_scheduler_contract::ScheduleExecution {
        ryeos_scheduler_contract::ScheduleExecution {
            authority: ryeos_scheduler_contract::ScheduleExecutionAuthority::Node {
                principal_id: format!("fp:{}", "33".repeat(32)),
                effective_origin_site_id: "site:test".to_string(),
            },
            capabilities: vec!["ryeos.execute.directive.test".to_string()],
            policy: ryeos_engine::execution_contract::ExecutionPolicy::projectless(
                ryeos_engine::execution_contract::ExecutionResponse::Accepted,
            ),
            limits: None,
            max_fires: None,
            expires_at: None,
        }
    }

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
            execution: projectless_execution(),
        }
    }

    fn make_fire(scheduled_at: i64) -> FireRecord {
        let fire_id = format!("test@{scheduled_at}");
        FireRecord {
            thread_id: Some(super::super::types::thread_id_from_fire(&fire_id)),
            fire_id,
            schedule_id: "test".to_string(),
            scheduled_at,
            reserved_at: scheduled_at + 1,
            dispatched_at: Some(scheduled_at + 1),
            completed_at: Some(scheduled_at + 1),
            status: "completed".to_string(),
            trigger_reason: "normal".to_string(),
            outcome: Some("success".to_string()),
            signer_fingerprint: "11".repeat(32),
            schedule_spec_hash: "22".repeat(32),
            project_authority: Some(
                ryeos_state::objects::ExecutionProjectAuthority::projectless(
                    ryeos_state::objects::EnvironmentAuthority::None,
                )
                .unwrap(),
            ),
            admitted_capsule_hash: Some("44".repeat(32)),
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
        let target = parse_iso_ts("2028-01-01T00:00:00Z").unwrap();
        let plan = plan_schedule(&spec, None, target - 1_000);
        assert_eq!(plan.current_due_at, None);
        assert_eq!(plan.next_fire_at, Some(target));
        assert_eq!(plan.decision, "waiting_for_next_boundary");
    }

    #[test]
    fn max_fires_stops_planning() {
        let mut spec = make_spec("interval", "60", 1_000);
        spec.execution.max_fires = Some(2);
        assert_eq!(
            lifetime_horizon(&spec),
            Some((121_000, ScheduleAutoPauseReason::MaxFiresReached))
        );

        // The second boundary is still admitted.
        let plan = plan_schedule(&spec, Some(&make_fire(61_000)), 121_500);
        assert_eq!(plan.current_due_at, Some(121_000));
        assert_eq!(plan.lifetime_exhausted, None);

        // After it, nothing is planned and the schedule reports exhaustion.
        let plan = plan_schedule(&spec, Some(&make_fire(121_000)), 121_500);
        assert_eq!(plan.current_due_at, None);
        assert_eq!(plan.next_fire_at, None);
        assert_eq!(
            plan.lifetime_exhausted,
            Some(ScheduleAutoPauseReason::MaxFiresReached)
        );
        assert_eq!(plan.decision, "lifetime_exhausted");
    }

    #[test]
    fn max_fires_keeps_the_last_admitted_boundary_after_downtime() {
        let mut spec = make_spec("interval", "60", 1_000);
        spec.execution.max_fires = Some(2);
        // Down from just after the first fire until well past the bound: the
        // second boundary is the current one and goes to misfire policy;
        // later boundaries are never planned.
        let plan = plan_schedule(&spec, Some(&make_fire(61_000)), 500_000);
        assert_eq!(plan.current_due_at, Some(121_000));
        assert_eq!(plan.current_due_within_grace, Some(false));
        assert_eq!(plan.misfire_horizon_exclusive, Some(121_001));
        assert_eq!(plan.next_fire_at, None);
        assert_eq!(plan.lifetime_exhausted, None);
    }

    #[test]
    fn expires_at_stops_planning() {
        let mut spec = make_spec("interval", "60", 1_000);
        spec.execution.expires_at = Some("1970-01-01T00:02:30Z".to_string());
        assert_eq!(
            lifetime_horizon(&spec),
            Some((149_999, ScheduleAutoPauseReason::Expired))
        );
        let plan = plan_schedule(&spec, Some(&make_fire(61_000)), 100_000);
        assert_eq!(plan.next_fire_at, Some(121_000));
        let plan = plan_schedule(&spec, Some(&make_fire(121_000)), 200_000);
        assert_eq!(plan.current_due_at, None);
        assert_eq!(plan.next_fire_at, None);
        assert_eq!(
            plan.lifetime_exhausted,
            Some(ScheduleAutoPauseReason::Expired)
        );
    }

    #[test]
    fn the_earlier_lifetime_bound_names_the_reason() {
        let mut spec = make_spec("interval", "60", 1_000);
        spec.execution.max_fires = Some(5);
        spec.execution.expires_at = Some("1970-01-01T00:02:30Z".to_string());
        assert_eq!(
            lifetime_horizon(&spec).map(|(_, reason)| reason),
            Some(ScheduleAutoPauseReason::Expired)
        );
        spec.execution.max_fires = Some(1);
        assert_eq!(
            lifetime_horizon(&spec),
            Some((61_000, ScheduleAutoPauseReason::MaxFiresReached))
        );
    }

    #[test]
    fn unbounded_schedule_has_no_lifetime_horizon() {
        let spec = make_spec("interval", "60", 1_000);
        assert_eq!(lifetime_horizon(&spec), None);
        assert_eq!(clamp_to_lifetime_exclusive(&spec, 9_000_000), 9_000_000);
        let plan = plan_schedule(&spec, None, 500_000);
        assert_eq!(plan.lifetime_exhausted, None);
    }

    #[test]
    fn at_recorded_schedule_is_exhausted() {
        let spec = make_spec("at", "2028-01-01T00:00:00Z", 1_000);
        let target = parse_iso_ts("2028-01-01T00:00:00Z").unwrap();
        let fire = make_fire(target);
        let plan = plan_schedule(&spec, Some(&fire), target + 1_000);
        assert_eq!(plan.current_due_at, None);
        assert_eq!(plan.next_fire_at, None);
        assert_eq!(plan.decision, "exhausted");
    }
}
