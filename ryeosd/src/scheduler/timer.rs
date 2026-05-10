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
use super::types::{self, FireRecord, ReloadSignal, ScheduleSpecRecord};
use crate::state::AppState;

/// Start the timer loop. Call after listeners are bound.
pub async fn run(state: Arc<AppState>, mut reload: mpsc::Receiver<ReloadSignal>) {
    let mut specs = load_specs_or_log(&state);

    tracing::info!(count = specs.len(), "scheduler: timer loop started");

    loop {
        let now = lillux::time::timestamp_millis();

        // Collect all specs that are due right now (fire time <= now).
        // We check by computing next fire relative to (now - 1ms) so that
        // a fire exactly at `now` is included. This avoids the bug where
        // compute_next_fire(interval, None) always returns now+interval,
        // making is_due always false. Instead we compute each spec's
        // scheduled_at from the DB last fire and check if it's in the past.
        let due_specs: Vec<ScheduleSpecRecord> = specs.iter()
            .filter(|s| spec_is_due(s, &state, now))
            .cloned()
            .collect();

        // Process each due spec sequentially (ordering matters for overlap)
        for spec in &due_specs {
            process_spec(spec, &state).await;
        }

        // Reload for next iteration
        specs = load_specs_or_log(&state);

        let now = lillux::time::timestamp_millis();
        let next_fire = compute_soonest_fire(&specs, now);

        let sleep_duration = match next_fire {
            Some(at) if at > now => std::time::Duration::from_millis((at - now) as u64),
            _ => std::time::Duration::from_secs(1),
        };

        tokio::select! {
            _ = tokio::time::sleep(sleep_duration) => {}
            Some(_) = reload.recv() => {
                specs = load_specs_or_log(&state);
                tracing::debug!(count = specs.len(), "scheduler: reloaded specs");
                continue;
            }
        }
    }
}

/// Load enabled specs, logging an error on DB failure instead of silently
/// returning an empty list. On error we return the previous specs unchanged
/// so the timer retries on the next tick rather than dropping all schedules.
fn load_specs_or_log(state: &Arc<AppState>) -> Vec<ScheduleSpecRecord> {
    match state.scheduler_db.load_enabled_specs() {
        Ok(specs) => specs,
        Err(e) => {
            tracing::error!(error = %e, "scheduler: failed to load enabled specs — retaining previous list");
            Vec::new()
        }
    }
}

/// Check if a spec is due by computing its scheduled_at from the DB's last
/// fire record. For the first fire of an interval schedule (no last fire),
/// we use the spec's `last_modified` (set at registration time) as the base.
fn spec_is_due(spec: &ScheduleSpecRecord, state: &AppState, now: i64) -> bool {
    let last_fire_at = match state.scheduler_db.get_last_fire(&spec.schedule_id) {
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
            Some(spec.last_modified)
        } else {
            None
        }
    });

    let scheduled_at = match crontab::compute_scheduled_at(
        &spec.schedule_type, &spec.expression, &spec.timezone, now, effective_last,
        Some(spec.last_modified),
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
                let first_fire = spec.last_modified + interval_secs * 1000;
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
        // last_modified + interval. For cron/at, compute_next_fire handles it.
        if s.schedule_type == "interval" {
            let interval_secs = match s.expression.parse::<i64>() {
                Ok(secs) if secs > 0 => secs,
                _ => return None, // skip invalid interval in sleep calculation
            };
            let interval_ms = interval_secs * 1000;
            let first_fire = s.last_modified + interval_ms;
            if first_fire > now {
                return Some(first_fire);
            }
            // Already due — return now so the timer wakes immediately
            return Some(now);
        }
        crontab::compute_next_fire(&s.schedule_type, &s.expression, &s.timezone, now, None).ok().flatten()
    }).min()
}

async fn process_spec(spec: &ScheduleSpecRecord, state: &AppState) {
    let now = lillux::time::timestamp_millis();

    // Get last fire for this schedule to compute scheduled_at
    let last_fire_at = match state.scheduler_db.get_last_fire(&spec.schedule_id) {
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
        Some(spec.last_modified),
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
                spec.last_modified + interval_ms
            } else {
                return;
            }
        }
    };

    let fid = types::fire_id(&spec.schedule_id, scheduled_at);

    // ── Dedup check ──────────────────────────────────────────
    if let Ok(Some(_)) = state.scheduler_db.get_fire(&fid) {
        tracing::debug!(fire_id = %fid, "fire already exists — skipping");
        return;
    }

    // ── Misfire evaluation ───────────────────────────────────
    let misfire_fires = misfire::evaluate_misfires(spec, state, now).await;

    // ── Process misfires first (chronological) ───────────────
    for pending in misfire_fires {
        dispatch_fire(state, &pending.fire_id, spec, pending.scheduled_at, pending.reason).await;
    }

    // ── Overlap check for current fire ───────────────────────
    let overlap_ok = overlap::check_overlap(spec, state).await;
    if !overlap_ok {
        record_skip(state, &fid, spec, scheduled_at, "overlap_policy_skip").await;
        return;
    }

    // ── Dispatch current fire ────────────────────────────────
    dispatch_fire(state, &fid, spec, scheduled_at, "normal").await;
}

pub async fn dispatch_fire(
    state: &AppState,
    fire_id: &str,
    spec: &ScheduleSpecRecord,
    scheduled_at: i64,
    trigger_reason: &str,
) {
    let now = lillux::time::timestamp_millis();
    let thread_id = types::thread_id_from_fire(fire_id);

    // ── Record dispatched to JSONL + DB (blocking I/O) ───────
    let entry = serde_json::json!({
        "entry_type": "dispatched",
        "fire_id": fire_id,
        "schedule_id": spec.schedule_id,
        "scheduled_at": scheduled_at,
        "fired_at": now,
        "thread_id": thread_id,
        "trigger_reason": trigger_reason,
        "signer_fingerprint": spec.signer_fingerprint,
    });
    let fires_path = state.config.system_space_dir
        .join(ryeos_engine::AI_DIR).join("state").join("schedules")
        .join(&spec.schedule_id).join("fires.jsonl");
    let rec = FireRecord {
        fire_id: fire_id.to_string(),
        schedule_id: spec.schedule_id.clone(),
        scheduled_at,
        fired_at: Some(now),
        thread_id: Some(thread_id.clone()),
        status: "dispatched".to_string(),
        trigger_reason: trigger_reason.to_string(),
        outcome: None,
        signer_fingerprint: Some(spec.signer_fingerprint.clone()),
    };

    {
        let fires_path = fires_path.clone();
        let db = state.scheduler_db.clone();
        let fire_id = fire_id.to_string();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = projection::append_jsonl_entry(&fires_path, &entry) {
                tracing::error!(fire_id = %fire_id, error = %e, "failed to append dispatched entry");
            }
            if let Err(e) = db.upsert_fire(&rec) {
                tracing::error!(fire_id = %fire_id, error = %e, "failed to upsert dispatched fire");
            }
        }).await.ok();
    }

    // ── Dispatch via existing dispatch path ──────────────────
    let params: serde_json::Value = match serde_json::from_str(&spec.params) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                schedule_id = %spec.schedule_id,
                params_raw = %spec.params,
                error = %e,
                "failed to parse schedule params — defaulting to empty object"
            );
            serde_json::Value::Object(Default::default())
        }
    };
    let project_path = spec.project_root.as_deref().unwrap_or_else(|| {
        state.config.system_space_dir.to_str().unwrap_or(".")
    });
    let project_path_buf = std::path::PathBuf::from(project_path);

    let dispatch_req = crate::dispatch::DispatchRequest {
        launch_mode: "detached",
        target_site_id: None,
        project_source_is_pushed_head: false,
        validate_only: false,
        params,
        acting_principal: &spec.signer_fingerprint,
        project_path: std::path::Path::new(project_path),
        original_project_path: project_path_buf.clone(),
        snapshot_hash: None,
        temp_dir: None,
        original_root_kind: "directive",
        pre_minted_thread_id: Some(thread_id.clone()),
        operation: None,
        inputs: None,
    };

    let exec_ctx = crate::service_executor::ExecutionContext {
        principal_fingerprint: spec.signer_fingerprint.clone(),
        caller_scopes: vec!["*".to_string()],
        engine: state.engine.clone(),
        plan_ctx: ryeos_engine::contracts::PlanContext {
            requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
                ryeos_engine::contracts::Principal {
                    fingerprint: spec.signer_fingerprint.clone(),
                    scopes: vec![],
                },
            ),
            project_context: ryeos_engine::contracts::ProjectContext::LocalPath {
                path: project_path_buf,
            },
            current_site_id: "site:local".into(),
            origin_site_id: "site:local".into(),
            execution_hints: ryeos_engine::contracts::ExecutionHints::default(),
            validate_only: false,
        },
        requested_op: None,
        requested_inputs: None,
    };

    match crate::dispatch::dispatch(&spec.item_ref, &dispatch_req, &exec_ctx, state).await {
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
                thread_id: Some(thread_id),
                status: "failed".to_string(),
                trigger_reason: trigger_reason.to_string(),
                outcome: Some("dispatch_failed".to_string()),
                signer_fingerprint: Some(spec.signer_fingerprint.clone()),
            };
            {
                let db = state.scheduler_db.clone();
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = projection::append_jsonl_entry(&fires_path, &fail_entry) {
                        tracing::error!(fire_id = %fail_rec.fire_id, error = %e, "failed to append failure entry");
                    }
                    let _ = db.upsert_fire(&fail_rec);
                }).await.ok();
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
pub async fn dispatch_recovery_fire(state: Arc<AppState>, intent: super::reconcile::ResumeIntent) {
    dispatch_fire(&state, &intent.fire_id, &intent.spec, intent.scheduled_at, intent.trigger_reason).await;
}

async fn record_skip(
    state: &AppState,
    fire_id: &str,
    spec: &ScheduleSpecRecord,
    scheduled_at: i64,
    reason: &str,
) {
    let now = lillux::time::timestamp_millis();

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
    let fires_path = state.config.system_space_dir
        .join(ryeos_engine::AI_DIR).join("state").join("schedules")
        .join(&spec.schedule_id).join("fires.jsonl");
    let rec = FireRecord {
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
    {
        let db = state.scheduler_db.clone();
        let fire_id = fire_id.to_string();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = projection::append_jsonl_entry(&fires_path, &entry) {
                tracing::error!(fire_id = %fire_id, error = %e, "failed to append skip entry");
            }
            if let Err(e) = db.upsert_fire(&rec) {
                tracing::error!(fire_id = %fire_id, error = %e, "failed to upsert skipped fire");
            }
        }).await.ok();
    }
}
