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
    let mut specs = state.scheduler_db.load_enabled_specs()
        .unwrap_or_default();

    tracing::info!(count = specs.len(), "scheduler: timer loop started");

    loop {
        let now = lillux::time::timestamp_millis();
        let next_fire = compute_soonest_fire(&specs, now);

        let sleep_duration = match next_fire {
            Some(at) if at > now => std::time::Duration::from_millis((at - now) as u64),
            _ => std::time::Duration::from_secs(1),
        };

        tokio::select! {
            _ = tokio::time::sleep(sleep_duration) => {}
            Some(_) = reload.recv() => {
                specs = state.scheduler_db.load_enabled_specs()
                    .unwrap_or_default();
                tracing::debug!(count = specs.len(), "scheduler: reloaded specs");
                continue;
            }
        }

        let now = lillux::time::timestamp_millis();
        let due_specs: Vec<&ScheduleSpecRecord> = specs.iter()
            .filter(|s| is_due(s, now))
            .collect();

        // Process each due spec sequentially (ordering matters for overlap)
        for spec in &due_specs {
            process_spec(spec, &state).await;
        }

        // Reload for next iteration
        specs = state.scheduler_db.load_enabled_specs()
            .unwrap_or_default();
    }
}

fn compute_soonest_fire(specs: &[ScheduleSpecRecord], now: i64) -> Option<i64> {
    specs.iter().filter_map(|s| {
        let last = stateless_last_fire_at(s);
        crontab::compute_next_fire(&s.schedule_type, &s.expression, &s.timezone, now, last).ok().flatten()
    }).min()
}

fn is_due(spec: &ScheduleSpecRecord, now: i64) -> bool {
    let last = stateless_last_fire_at(spec);
    crontab::is_due(&spec.schedule_type, &spec.expression, &spec.timezone, now, last)
}

fn stateless_last_fire_at(_spec: &ScheduleSpecRecord) -> Option<i64> {
    // We don't query DB here (called in a filter). The timer loop will
    // do the DB dedup check inside process_spec.
    None
}

async fn process_spec(spec: &ScheduleSpecRecord, state: &AppState) {
    let now = lillux::time::timestamp_millis();

    // Get last fire for this schedule to compute scheduled_at
    let last_fire_at = state.scheduler_db.get_last_fire(&spec.schedule_id)
        .ok()
        .flatten()
        .map(|f| f.scheduled_at);

    let scheduled_at = match crontab::compute_scheduled_at(
        &spec.schedule_type, &spec.expression, &spec.timezone, now, last_fire_at,
    ) {
        Some(at) => at,
        None => return,
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

    // ── Record dispatched to JSONL ───────────────────────────
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
        .join(".ai").join("state").join("schedules")
        .join(&spec.schedule_id).join("fires.jsonl");
    if let Err(e) = projection::append_jsonl_entry(&fires_path, &entry) {
        tracing::error!(fire_id = %fire_id, error = %e, "failed to append dispatched entry");
    }

    // ── Update projection DB ─────────────────────────────────
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
    if let Err(e) = state.scheduler_db.upsert_fire(&rec) {
        tracing::error!(fire_id = %fire_id, error = %e, "failed to upsert dispatched fire");
    }

    // ── Dispatch via existing dispatch path ──────────────────
    let params: serde_json::Value = serde_json::from_str(&spec.params).unwrap_or_default();
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
            if let Err(e) = projection::append_jsonl_entry(&fires_path, &fail_entry) {
                tracing::error!(fire_id = %fire_id, error = %e, "failed to append failure entry");
            }

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
            let _ = state.scheduler_db.upsert_fire(&fail_rec);

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
        .join(".ai").join("state").join("schedules")
        .join(&spec.schedule_id).join("fires.jsonl");
    if let Err(e) = projection::append_jsonl_entry(&fires_path, &entry) {
        tracing::error!(fire_id = %fire_id, error = %e, "failed to append skip entry");
    }

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
    if let Err(e) = state.scheduler_db.upsert_fire(&rec) {
        tracing::error!(fire_id = %fire_id, error = %e, "failed to upsert skipped fire");
    }
}
