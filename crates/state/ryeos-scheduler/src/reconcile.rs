//! Scheduler reconciliation — crash recovery.
//!
//! Follows the existing thread reconciler pattern:
//! 1. Rebuild projection from CAS (YAML + JSONL)
//! 2. Recover in-flight fires (check thread status)
//! 3. Evaluate misfire gaps
//! 4. Collect intents to dispatch after listeners are bound

use anyhow::Result;

use super::misfire;
use super::projection;
use super::types::{FireRecord, ScheduleSpecRecord};
use crate::SchedulerContext;

/// A fire to dispatch after listeners are bound.
#[derive(Debug, Clone)]
pub struct ResumeIntent {
    pub fire_id: String,
    pub spec: ScheduleSpecRecord,
    pub scheduled_at: i64,
    pub trigger_reason: &'static str,
}

/// Full reconciliation sequence. Run after `SchedulerDb` is open, before timer starts.
#[tracing::instrument(name = "scheduler:reconcile", skip(ctx))]
pub async fn reconcile<Ctx: SchedulerContext>(ctx: &Ctx) -> Result<Vec<ResumeIntent>> {
    tracing::info!("scheduler: starting reconciliation");

    // Step 1: Rebuild projection from CAS
    let schedules_dir = ctx
        .system_space_dir()
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("schedules");
    let fires_dir = ctx
        .system_space_dir()
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join("schedules");

    let db = ctx.scheduler_db();
    let live_ids =
        projection::rebuild_specs_from_dir(&schedules_dir, &db, Some(ctx.trust_store()))?;

    // Delete projections for removed YAML files
    let live_refs: Vec<&str> = live_ids.iter().map(|s| s.as_str()).collect();
    let removed = db.delete_stale_specs(&live_refs)?;
    if removed > 0 {
        tracing::info!(removed, "scheduler: removed stale spec projections");
    }

    projection::rebuild_fires_from_dir(&fires_dir, &db)?;
    tracing::info!(
        specs = live_ids.len(),
        "scheduler: projection rebuilt from CAS"
    );

    // Step 2: Recover in-flight fires
    let mut intents = recover_inflight_fires(ctx).await?;
    tracing::info!(
        inflight = intents.len(),
        "scheduler: recovered in-flight fires"
    );

    // Step 3: Evaluate misfire gaps
    let specs = db.load_enabled_specs()?;
    let now = lillux::time::timestamp_millis();

    for spec in &specs {
        let last_fire_at = match db.get_last_fire(&spec.schedule_id) {
            Ok(Some(f)) => Some(f.scheduled_at),
            Ok(None) => None,
            Err(e) => {
                tracing::error!(
                    schedule_id = %spec.schedule_id,
                    error = %e,
                    "reconcile: failed to query last fire — skipping misfire evaluation"
                );
                continue;
            }
        };

        // evaluate_misfires internally calls detect_misfires and handles
        // the gap detection + policy evaluation. Only call it if we have
        // a previous fire (otherwise there's no gap to evaluate).
        if last_fire_at.is_some() {
            let pending = misfire::evaluate_misfires(spec, ctx, now).await;
            for p in pending {
                intents.push(ResumeIntent {
                    fire_id: p.fire_id,
                    spec: spec.clone(),
                    scheduled_at: p.scheduled_at,
                    trigger_reason: p.reason,
                });
            }
        }
    }

    tracing::info!(
        intents = intents.len(),
        "scheduler: reconciliation complete"
    );
    Ok(intents)
}

async fn recover_inflight_fires<Ctx: SchedulerContext>(ctx: &Ctx) -> Result<Vec<ResumeIntent>> {
    let db = ctx.scheduler_db();
    let inflight = db.get_inflight_fires()?;
    let mut intents = Vec::new();

    for fire in &inflight {
        match &fire.thread_id {
            Some(thread_id) => {
                match ctx.get_thread_status(thread_id) {
                    Ok(Some(status)) => match status.as_str() {
                        "completed" => {
                            update_fire_completed(ctx, fire, thread_id, "completed", "success")
                                .await?;
                            tracing::info!(
                                fire_id = %fire.fire_id,
                                thread_id = %thread_id,
                                "recovered: thread completed during downtime"
                            );
                        }
                        "failed" => {
                            update_fire_completed(ctx, fire, thread_id, "failed", "thread_failed")
                                .await?;
                            tracing::info!(
                                fire_id = %fire.fire_id,
                                thread_id = %thread_id,
                                "recovered: thread failed during downtime"
                            );
                        }
                        "cancelled" => {
                            update_fire_completed(
                                ctx,
                                fire,
                                thread_id,
                                "cancelled",
                                "thread_cancelled",
                            )
                            .await?;
                            tracing::info!(
                                fire_id = %fire.fire_id,
                                thread_id = %thread_id,
                                "recovered: thread cancelled during downtime"
                            );
                        }
                        status => {
                            tracing::warn!(
                                fire_id = %fire.fire_id,
                                thread_id = %thread_id,
                                status = %status,
                                "recovered: thread in non-terminal status — existing reconciler handles"
                            );
                        }
                    },
                    Ok(None) => {
                        // Thread row gone — try to redispatch if schedule exists
                        let spec = db.get_spec(&fire.schedule_id)?;
                        match spec {
                            Some(spec) if spec.enabled => {
                                tracing::warn!(
                                    fire_id = %fire.fire_id,
                                    "recovered: thread row missing — collecting for redispatch"
                                );
                                intents.push(ResumeIntent {
                                    fire_id: fire.fire_id.clone(),
                                    spec,
                                    scheduled_at: fire.scheduled_at,
                                    trigger_reason: "recovery_thread_lost",
                                });
                            }
                            _ => {
                                let fail_rec = FireRecord {
                                    status: "failed".to_string(),
                                    outcome: Some("thread_lost".to_string()),
                                    fired_at: Some(lillux::time::timestamp_millis()),
                                    completed_at: None,
                                    ..fire.clone()
                                };
                                db.upsert_fire(&fail_rec)?;
                                tracing::warn!(
                                    fire_id = %fire.fire_id,
                                    "recovered: thread row missing, schedule removed — marking fire as failed"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            fire_id = %fire.fire_id,
                            error = %e,
                            "recovered: error checking thread status"
                        );
                    }
                }
            }
            None => {
                // Dispatch was interrupted — try to redispatch
                let spec = db.get_spec(&fire.schedule_id)?;
                match spec {
                    Some(spec) if spec.enabled => {
                        tracing::info!(
                            fire_id = %fire.fire_id,
                            "recovered: interrupted dispatch — collecting for redispatch"
                        );
                        intents.push(ResumeIntent {
                            fire_id: fire.fire_id.clone(),
                            spec,
                            scheduled_at: fire.scheduled_at,
                            trigger_reason: "recovery",
                        });
                    }
                    _ => {
                        let skip_rec = FireRecord {
                            status: "skipped".to_string(),
                            outcome: Some("recovery_schedule_removed".to_string()),
                            completed_at: None,
                            ..fire.clone()
                        };
                        db.upsert_fire(&skip_rec)?;
                        tracing::info!(
                            fire_id = %fire.fire_id,
                            "recovered: schedule removed — skipping interrupted fire"
                        );
                    }
                }
            }
        }
    }

    Ok(intents)
}

async fn update_fire_completed<Ctx: SchedulerContext>(
    ctx: &Ctx,
    fire: &FireRecord,
    thread_id: &str,
    status: &str,
    outcome: &str,
) -> Result<()> {
    let now = lillux::time::timestamp_millis();

    let rec = FireRecord {
        status: status.to_string(),
        outcome: Some(outcome.to_string()),
        fired_at: Some(now),
        completed_at: Some(now),
        ..fire.clone()
    };

    let entry = serde_json::json!({
        "entry_type": status,
        "status": status,
        "fire_id": fire.fire_id,
        "schedule_id": fire.schedule_id,
        "scheduled_at": fire.scheduled_at,
        "fired_at": fire.fired_at.unwrap_or(now),
        "thread_id": thread_id,
        "completed_at": now,
        "outcome": outcome,
        "signer_fingerprint": fire.signer_fingerprint,
    });
    let fires_path = ctx
        .system_space_dir()
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join("schedules")
        .join(&fire.schedule_id)
        .join("fires.jsonl");

    let db = ctx.scheduler_db();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = db.upsert_fire(&rec) {
            tracing::error!(fire_id = %rec.fire_id, error = %e, "failed to update reconciled fire status");
        }
        if let Err(e) = projection::append_jsonl_entry(&fires_path, &entry) {
            tracing::error!(fire_id = %rec.fire_id, error = %e, "failed to append reconciled fire entry");
        }
    }).await.map_err(|e| anyhow::anyhow!("spawn_blocking join error: {}", e))?;

    Ok(())
}
