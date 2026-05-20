//! Timer loop — the core scheduler.
//!
//! A `tokio::spawn` background task that sleeps until the next fire,
//! dispatches work, and listens for completion events.

use std::sync::Arc;

use tokio::sync::mpsc;

use super::crontab;
use super::misfire;
use super::overlap;
use super::projection;
use super::types::{self, FireRecord, PendingFire, ReloadSignal, ScheduleSpecRecord};
use crate::SchedulerContext;

/// Start the timer loop. Call after listeners are bound.
pub async fn run<Ctx: SchedulerContext>(ctx: Arc<Ctx>, mut reload: mpsc::Receiver<ReloadSignal>) {
    let mut specs = load_specs_or_log(&ctx);
    let mut last_repair = lillux::time::timestamp_millis();

    tracing::info!(count = specs.len(), "scheduler: timer loop started");

    loop {
        let now = lillux::time::timestamp_millis();

        // Periodic repair sweep: every 30 seconds, finalize stale dispatched fires
        if now - last_repair >= 30_000 {
            repair_stale_fires(&ctx).await;
            last_repair = now;
        }

        // Collect all specs that are due right now (fire time <= now).
        // We check by computing next fire relative to (now - 1ms) so that
        // a fire exactly at `now` is included. This avoids the bug where
        // compute_next_fire(interval, None) always returns now+interval,
        // making is_due always false. Instead we compute each spec's
        // scheduled_at from the DB last fire and check if it's in the past.
        let due_specs: Vec<ScheduleSpecRecord> = specs.iter()
            .filter(|s| spec_is_due(s, &ctx, now))
            .cloned()
            .collect();

        // Process each due spec sequentially (ordering matters for overlap)
        for spec in &due_specs {
            process_spec(spec, &ctx).await;
        }

        // Reload for next iteration
        specs = load_specs_or_log(&ctx);

        let now = lillux::time::timestamp_millis();
        let next_fire = compute_soonest_fire(&specs, now);

        let sleep_duration = match next_fire {
            Some(at) if at > now => std::time::Duration::from_millis((at - now) as u64),
            _ => std::time::Duration::from_secs(1),
        };

        tokio::select! {
            _ = tokio::time::sleep(sleep_duration) => {}
            Some(_) = reload.recv() => {
                specs = load_specs_or_log(&ctx);
                tracing::debug!(count = specs.len(), "scheduler: reloaded specs");
                continue;
            }
        }
    }
}

/// Repair sweep: finalize dispatched fires whose threads have completed.
/// Catches cases where the finalize hook failed or the daemon crashed.
async fn repair_stale_fires<Ctx: SchedulerContext>(ctx: &Ctx) {
    let db = ctx.scheduler_db();
    let stale = match db.find_stale_dispatched_fires(30) {
        Ok(fires) => fires,
        Err(e) => {
            tracing::error!(error = %e, "scheduler: repair sweep failed to query stale fires");
            return;
        }
    };

    for fire in &stale {
        if let Some(ref thread_id) = fire.thread_id {
            match ctx.get_thread_status(thread_id) {
                Ok(Some(status)) => {
                    if overlap::thread_is_terminal(&status) {
                        let fire_status = match status.as_str() {
                            "completed" => "completed",
                            "cancelled" => "cancelled",
                            _ => "failed",
                        };
                        let outcome = match fire_status {
                            "completed" => "success",
                            "cancelled" => "thread_cancelled",
                            _ => "thread_failed",
                        };
                        let now = lillux::time::timestamp_millis();
                        let rec = FireRecord {
                            status: fire_status.to_string(),
                            outcome: Some(outcome.to_string()),
                            completed_at: Some(now),
                            ..fire.clone()
                        };
                        let entry = serde_json::json!({
                            "entry_type": "repaired",
                            "status": fire_status,
                            "fire_id": fire.fire_id,
                            "schedule_id": fire.schedule_id,
                            "scheduled_at": fire.scheduled_at,
                            "fired_at": fire.fired_at.unwrap_or(now),
                            "thread_id": thread_id,
                            "completed_at": now,
                            "outcome": outcome,
                            "trigger_reason": fire.trigger_reason,
                            "signer_fingerprint": fire.signer_fingerprint,
                        });
                        let fires_path = ctx.system_space_dir()
                            .join(ryeos_engine::AI_DIR).join("state").join("schedules")
                            .join(&fire.schedule_id).join("fires.jsonl");
                        let db = ctx.scheduler_db();
                        let fire_id_owned = fire.fire_id.clone();
                        let fire_id_log = fire.fire_id.clone();
                        tokio::task::spawn_blocking(move || {
                            if let Err(e) = projection::append_jsonl_entry(&fires_path, &entry) {
                                tracing::error!(fire_id = %fire_id_owned, error = %e,
                                    "repair sweep: failed to append JSONL entry");
                            }
                            if let Err(e) = db.upsert_fire(&rec) {
                                tracing::error!(fire_id = %fire_id_owned, error = %e,
                                    "scheduler: repair sweep failed to finalize fire");
                            }
                        }).await.unwrap_or_else(|e| {
                            tracing::error!(fire_id = %fire_id_log, error = %e,
                                "repair sweep: spawn_blocking failed");
                        });
                        tracing::info!(
                            fire_id = %fire.fire_id,
                            thread_id = %thread_id,
                            status = %fire_status,
                            "scheduler: repair sweep finalized stale fire"
                        );
                    }
                }
                Ok(None) => {
                    // Thread gone — mark as failed
                    let now = lillux::time::timestamp_millis();
                    let rec = FireRecord {
                        status: "failed".to_string(),
                        outcome: Some("thread_lost".to_string()),
                        completed_at: Some(now),
                        ..fire.clone()
                    };
                    let entry = serde_json::json!({
                        "entry_type": "repaired",
                        "status": "failed",
                        "fire_id": fire.fire_id,
                        "schedule_id": fire.schedule_id,
                        "scheduled_at": fire.scheduled_at,
                        "fired_at": fire.fired_at.unwrap_or(now),
                        "thread_id": thread_id,
                        "completed_at": now,
                        "outcome": "thread_lost",
                        "trigger_reason": fire.trigger_reason,
                        "signer_fingerprint": fire.signer_fingerprint,
                    });
                    let fires_path = ctx.system_space_dir()
                        .join(ryeos_engine::AI_DIR).join("state").join("schedules")
                        .join(&fire.schedule_id).join("fires.jsonl");
                    let db = ctx.scheduler_db();
                    let fire_id_owned = fire.fire_id.clone();
                    let fire_id_log = fire.fire_id.clone();
                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = projection::append_jsonl_entry(&fires_path, &entry) {
                            tracing::error!(fire_id = %fire_id_owned, error = %e,
                                "repair sweep: failed to append thread_lost JSONL entry");
                        }
                        if let Err(e) = db.upsert_fire(&rec) {
                            tracing::error!(fire_id = %fire_id_owned, error = %e,
                                "repair sweep: failed to upsert thread_lost fire record");
                        }
                    }).await.unwrap_or_else(|e| {
                        tracing::error!(fire_id = %fire_id_log, error = %e,
                            "repair sweep: spawn_blocking failed for thread_lost");
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        fire_id = %fire.fire_id,
                        error = %e,
                        "scheduler: repair sweep failed to check thread"
                    );
                }
            }
        }
    }
}

/// Load enabled specs from DB. On failure, return empty vec and log error.
///
/// Empty vec causes the timer to temporarily pause — no schedules appear due,
/// so no fires are dispatched. On the next successful load cycle (1s sleep),
/// real schedules reappear. This prevents cascading failures when the DB is
/// temporarily unavailable.
fn load_specs_or_log<Ctx: SchedulerContext>(ctx: &Arc<Ctx>) -> Vec<ScheduleSpecRecord> {
    match ctx.scheduler_db().load_enabled_specs() {
        Ok(specs) => specs,
        Err(e) => {
            tracing::error!(
                error = %e,
                "scheduler: failed to load enabled specs — timer paused until next load cycle"
            );
            Vec::new()
        }
    }
}

/// Check if a spec is due by computing its scheduled_at from the DB's last
/// fire record. For the first fire of an interval schedule (no last fire),
/// we use the spec's `registered_at` (immutable anchor) as the base.
fn spec_is_due<Ctx: SchedulerContext>(spec: &ScheduleSpecRecord, ctx: &Ctx, now: i64) -> bool {
    let db = ctx.scheduler_db();
    let last_fire_at = match db.get_last_fire(&spec.schedule_id) {
        Ok(Some(f)) => Some(f.scheduled_at),
        Ok(None) => None,
        Err(e) => {
            tracing::error!(
                schedule_id = %spec.schedule_id,
                error = %e,
                "spec_is_due: failed to query last fire — treating as no prior fire"
            );
            None
        }
    };

    // For interval schedules with no prior fire, use registration time as base
    let effective_last = last_fire_at.or_else(|| {
        if spec.schedule_type == "interval" || spec.schedule_type == "cron" {
            Some(spec.registered_at)
        } else {
            None
        }
    });

    let scheduled_at = match crontab::compute_scheduled_at(
        &spec.schedule_type, &spec.expression, &spec.timezone, now, effective_last,
        Some(spec.registered_at),
    ) {
        Some(at) => at,
        None => {
            // For interval with no last fire and no effective_last, the first
            // fire is at registration_time + interval. compute_scheduled_at
            // returns None when last_fire_at is None, so handle it here.
            if spec.schedule_type == "interval" {
                let interval_secs = match spec.expression.parse::<i64>() {
                    Ok(secs) if secs > 0 => secs,
                    Ok(_) => {
                        tracing::error!(
                            schedule_id = %spec.schedule_id,
                            expression = %spec.expression,
                            "spec_is_due: interval expression is zero or negative — skipping"
                        );
                        return false;
                    }
                    Err(e) => {
                        tracing::error!(
                            schedule_id = %spec.schedule_id,
                            expression = %spec.expression,
                            error = %e,
                            "spec_is_due: interval expression failed to parse — skipping"
                        );
                        return false;
                    }
                };
                let first_fire = spec.registered_at + interval_secs * 1000;
                return first_fire <= now;
            }
            // For at schedules, parse the expression directly
            if spec.schedule_type == "at" {
                return crontab::parse_iso_ts(&spec.expression)
                    .is_some_and(|ts| ts <= now);
            }
            return false;
        }
    };

    // Due if the scheduled time has passed
    scheduled_at <= now
}

fn compute_soonest_fire(specs: &[ScheduleSpecRecord], now: i64) -> Option<i64> {
    specs.iter().filter_map(|s| {
        // For interval schedules with no prior fires, the first fire is at
        // registered_at + interval. For cron/at, compute_next_fire handles it.
        if s.schedule_type == "interval" {
            let interval_secs = match s.expression.parse::<i64>() {
                Ok(secs) if secs > 0 => secs,
                _ => return None, // skip invalid interval in sleep calculation
            };
            let interval_ms = interval_secs * 1000;
            let first_fire = s.registered_at + interval_ms;
            if first_fire > now {
                return Some(first_fire);
            }
            // Already due — return now so the timer wakes immediately
            return Some(now);
        }
        match crontab::compute_next_fire(&s.schedule_type, &s.expression, &s.timezone, now, None) {
            Ok(Some(at)) => Some(at),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(
                    schedule_id = %s.schedule_id,
                    expression = %s.expression,
                    error = %e,
                    "compute_soonest_fire: invalid expression — using 1s fallback"
                );
                None
            }
        }
    }).min()
}

async fn process_spec<Ctx: SchedulerContext>(spec: &ScheduleSpecRecord, ctx: &Ctx) {
    let now = lillux::time::timestamp_millis();

    // Get last fire for this schedule to compute scheduled_at
    let db = ctx.scheduler_db();
    let last_fire_at = match db.get_last_fire(&spec.schedule_id) {
        Ok(Some(f)) => Some(f.scheduled_at),
        Ok(None) => None,
        Err(e) => {
            tracing::error!(
                schedule_id = %spec.schedule_id,
                error = %e,
                "process_spec: failed to query last fire — treating as no prior fire"
            );
            None
        }
    };

    let scheduled_at = match crontab::compute_scheduled_at(
        &spec.schedule_type, &spec.expression, &spec.timezone, now, last_fire_at,
        Some(spec.registered_at),
    ) {
        Some(at) => at,
        None => {
            // Fallback for interval first-fire: registration_time + interval
            if spec.schedule_type == "interval" {
                let interval_ms = match spec.expression.parse::<i64>() {
                    Ok(secs) if secs > 0 => secs * 1000,
                    _ => {
                        tracing::error!(
                            schedule_id = %spec.schedule_id,
                            expression = %spec.expression,
                            "process_spec: invalid interval expression — skipping fire"
                        );
                        return;
                    }
                };
                spec.registered_at + interval_ms
            } else {
                return;
            }
        }
    };

    let fid = types::fire_id(&spec.schedule_id, scheduled_at);

    // ── Misfire evaluation ───────────────────────────────────
    let mut misfire_fires = misfire::evaluate_misfires(spec, ctx, now).await;

    // Gap 4.3: Exclude the current fire from misfires if it's there
    misfire_fires.retain(|p| p.scheduled_at != scheduled_at);

    // ── Build one ordered fire list (chronological) ──────────
    // Each fire gets its own overlap check before dispatch.
    let mut all_fires: Vec<PendingFire> = misfire_fires;
    all_fires.push(PendingFire {
        fire_id: fid,
        scheduled_at,
        reason: "normal",
    });
    all_fires.sort_by_key(|f| f.scheduled_at);

    // ── Process each fire with overlap check ─────────────────
    for pending in all_fires {
        let overlap_ok = overlap::check_overlap(spec, ctx).await;
        if !overlap_ok {
            // Skip this fire and all subsequent fires
            record_skip(ctx, &pending.fire_id, spec, pending.scheduled_at, "overlap_policy_skip").await;
            break; // stop processing this batch, later fires reappear next tick
        }
        dispatch_fire(ctx, &pending.fire_id, spec, pending.scheduled_at, pending.reason, false).await;
    }
}

pub async fn dispatch_fire<Ctx: SchedulerContext>(
    ctx: &Ctx,
    fire_id: &str,
    spec: &ScheduleSpecRecord,
    scheduled_at: i64,
    trigger_reason: &str,
    skip_claim: bool,
) {
    // Fail-closed: never dispatch with empty capabilities.
    // If capabilities are empty, the schedule is corrupted or misconfigured.
    if spec.capabilities.is_empty() {
        tracing::error!(
            schedule_id = %spec.schedule_id,
            fire_id = %fire_id,
            "refusing to dispatch schedule with empty capabilities — \
             corrupted or misconfigured schedule. Deregister and re-register."
        );
        let now = lillux::time::timestamp_millis();
        let thread_id = types::thread_id_from_fire(fire_id);
        let fail_entry = serde_json::json!({
            "entry_type": "failed",
            "status": "failed",
            "fire_id": fire_id,
            "schedule_id": spec.schedule_id,
            "scheduled_at": scheduled_at,
            "fired_at": now,
            "thread_id": thread_id,
            "completed_at": now,
            "outcome": "dispatch_skipped",
            "error": "empty capabilities",
            "signer_fingerprint": spec.signer_fingerprint,
        });
        let fail_rec = types::FireRecord {
            fire_id: fire_id.to_string(),
            schedule_id: spec.schedule_id.clone(),
            scheduled_at,
            fired_at: Some(now),
            completed_at: Some(now),
            thread_id: Some(thread_id),
            status: "failed".to_string(),
            trigger_reason: trigger_reason.to_string(),
            outcome: Some("dispatch_skipped".to_string()),
            signer_fingerprint: Some(spec.signer_fingerprint.clone()),
        };
        let db = ctx.scheduler_db();
        let fires_path = ctx.system_space_dir()
            .join(ryeos_engine::AI_DIR).join("state").join("schedules")
            .join(&spec.schedule_id).join("fires.jsonl");
        let fire_id_owned = fire_id.to_string();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = projection::append_jsonl_entry(&fires_path, &fail_entry) {
                tracing::error!(fire_id = %fire_id_owned, path = %fires_path.display(), error = %e,
                    "empty-caps failure: failed to append JSONL entry");
            }
            if let Err(e) = db.upsert_fire(&fail_rec) {
                tracing::error!(fire_id = %fire_id_owned, error = %e,
                    "empty-caps failure: failed to upsert fire record");
            }
        }).await.unwrap_or_else(|e| {
            tracing::error!(fire_id = %fire_id, error = %e, "spawn_blocking failed for empty-caps failure record");
        });
        return;
    }

    let now = lillux::time::timestamp_millis();
    let thread_id = types::thread_id_from_fire(fire_id);

    // ── Record dispatched to JSONL + DB (blocking I/O) ───────
    let entry = serde_json::json!({
        "entry_type": "dispatched",
        "status": "dispatched",
        "fire_id": fire_id,
        "schedule_id": spec.schedule_id,
        "scheduled_at": scheduled_at,
        "fired_at": now,
        "thread_id": thread_id,
        "trigger_reason": trigger_reason,
        "signer_fingerprint": spec.signer_fingerprint,
    });
    let fires_path = ctx.system_space_dir()
        .join(ryeos_engine::AI_DIR).join("state").join("schedules")
        .join(&spec.schedule_id).join("fires.jsonl");
    let rec = FireRecord {
        fire_id: fire_id.to_string(),
        schedule_id: spec.schedule_id.clone(),
        scheduled_at,
        fired_at: Some(now),
        completed_at: None,
        thread_id: Some(thread_id.clone()),
        status: "dispatched".to_string(),
        trigger_reason: trigger_reason.to_string(),
        outcome: None,
        signer_fingerprint: Some(spec.signer_fingerprint.clone()),
    };

    if !skip_claim {
        let fires_path = fires_path.clone();
        let db = ctx.scheduler_db();
        let fire_id_owned = fire_id.to_string();
        let claimed = tokio::task::spawn_blocking(move || {
            // Atomic claim: only proceed if this fire_id doesn't exist yet.
            // This prevents duplicate dispatch when recovery and timer race.
            match db.claim_fire(&rec) {
                Ok(true) => {
                    // Claimed — append to JSONL
                    if let Err(e) = projection::append_jsonl_entry(&fires_path, &entry) {
                        tracing::error!(fire_id = %fire_id_owned, error = %e, "failed to append dispatched entry");
                    }
                    true
                }
                Ok(false) => {
                    tracing::debug!(fire_id = %fire_id_owned, "fire already claimed — skipping dispatch");
                    false
                }
                Err(e) => {
                    tracing::error!(fire_id = %fire_id_owned, error = %e, "failed to claim fire — aborting dispatch");
                    false
                }
            }
        }).await.unwrap_or_else(|e| {
            tracing::error!(fire_id = %fire_id, error = %e, "spawn_blocking task panicked or was cancelled (dispatch claim)");
            false
        });

        if !claimed {
            return;
        }
    } else {
        // Recovery path: fire already reclaimed, just update JSONL + DB
        let fires_path = fires_path.clone();
        let db = ctx.scheduler_db();
        let fire_id_owned = fire_id.to_string();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = projection::append_jsonl_entry(&fires_path, &entry) {
                tracing::error!(fire_id = %fire_id_owned, error = %e, "failed to append recovery dispatch entry");
            }
            if let Err(e) = db.upsert_fire(&rec) {
                tracing::error!(fire_id = %fire_id_owned, error = %e, "failed to upsert recovery dispatch fire");
            }
        }).await.unwrap_or_else(|e| {
            tracing::error!(fire_id = %fire_id, error = %e, "spawn_blocking task panicked or was cancelled (recovery dispatch)");
        });
    }

    // ── Dispatch via SchedulerContext ────────────────────────
    match ctx.dispatch_scheduled_item(spec, fire_id, &thread_id, scheduled_at, trigger_reason).await {
        Ok(_) => {
            tracing::info!(
                fire_id = %fire_id,
                thread_id = %thread_id,
                trigger = %trigger_reason,
                schedule_id = %spec.schedule_id,
                "schedule fired"
            );
        }
        Err(err) => {
            let now = lillux::time::timestamp_millis();
            let fail_entry = serde_json::json!({
                "entry_type": "failed",
                "status": "failed",
                "fire_id": fire_id,
                "schedule_id": spec.schedule_id,
                "scheduled_at": scheduled_at,
                "fired_at": now,
                "thread_id": thread_id,
                "completed_at": now,
                "outcome": "dispatch_failed",
                "error": err.to_string(),
                "signer_fingerprint": spec.signer_fingerprint,
            });
            let fail_rec = FireRecord {
                fire_id: fire_id.to_string(),
                schedule_id: spec.schedule_id.clone(),
                scheduled_at,
                fired_at: Some(now),
                completed_at: Some(now),
                thread_id: Some(thread_id),
                status: "failed".to_string(),
                trigger_reason: trigger_reason.to_string(),
                outcome: Some("dispatch_failed".to_string()),
                signer_fingerprint: Some(spec.signer_fingerprint.clone()),
            };
            {
                let db = ctx.scheduler_db();
                let fires_path = fires_path.clone();
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = projection::append_jsonl_entry(&fires_path, &fail_entry) {
                        tracing::error!(fire_id = %fail_rec.fire_id, error = %e,
                            "dispatch failure: failed to append JSONL entry");
                    }
                    if let Err(e) = db.upsert_fire(&fail_rec) {
                        tracing::error!(fire_id = %fail_rec.fire_id, error = %e,
                            "dispatch failure: failed to upsert fire record");
                    }
                }).await.unwrap_or_else(|e| {
                    tracing::error!(fire_id = %fire_id, error = %e, "spawn_blocking task panicked or was cancelled (failure persist)");
                });
            }

            tracing::error!(
                fire_id = %fire_id,
                schedule_id = %spec.schedule_id,
                error = %err,
                "schedule dispatch failed"
            );
        }
    }
}

/// Dispatch a recovery fire (from reconciler).
/// Uses reclaim path instead of claim to handle fires already persisted in DB.
pub async fn dispatch_recovery_fire<Ctx: SchedulerContext>(ctx: Arc<Ctx>, intent: super::reconcile::ResumeIntent) {
    // Attempt to reclaim the fire for redispatch.
    // This handles the case where fire was persisted but thread was never created.
    let fire_id = intent.fire_id.clone();
    let db = ctx.scheduler_db();
    let can_reclaim = tokio::task::spawn_blocking(move || {
        db.reclaim_fire(&fire_id).unwrap_or(false)
    }).await.unwrap_or_else(|e| {
        tracing::error!(fire_id = %intent.fire_id, error = %e, "reclaim spawn_blocking failed");
        false
    });

    if !can_reclaim {
        tracing::warn!(
            fire_id = %intent.fire_id,
            "fire not reclaim-safe — skipping recovery dispatch"
        );
        return;
    }

    dispatch_fire(&ctx, &intent.fire_id, &intent.spec, intent.scheduled_at, intent.trigger_reason, true).await;
}

async fn record_skip<Ctx: SchedulerContext>(
    ctx: &Ctx,
    fire_id: &str,
    spec: &ScheduleSpecRecord,
    scheduled_at: i64,
    reason: &str,
) {
    let now = lillux::time::timestamp_millis();

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
    let fires_path = ctx.system_space_dir()
        .join(ryeos_engine::AI_DIR).join("state").join("schedules")
        .join(&spec.schedule_id).join("fires.jsonl");
    let rec = FireRecord {
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
    {
        let db = ctx.scheduler_db();
        let fire_id_owned = fire_id.to_string();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = projection::append_jsonl_entry(&fires_path, &entry) {
                tracing::error!(fire_id = %fire_id_owned, error = %e, "failed to append skip entry");
            }
            if let Err(e) = db.upsert_fire(&rec) {
                tracing::error!(fire_id = %fire_id_owned, error = %e, "failed to upsert skipped fire");
            }
        }).await.unwrap_or_else(|e| {
            tracing::error!(fire_id = %fire_id, error = %e, "spawn_blocking task panicked or was cancelled (skip persist)");
        });
    }
}
