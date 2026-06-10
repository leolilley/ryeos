//! Scheduler reconciliation — crash recovery.
//!
//! Follows the existing thread reconciler pattern:
//! 1. Rebuild projection from CAS (YAML + JSONL)
//! 2. Recover in-flight fires (check thread status)
//! 3. Evaluate misfire gaps
//! 4. Collect intents to dispatch after listeners are bound

use anyhow::Result;

use super::misfire;
use super::planning;
use super::projection;
use super::types::{FireRecord, ScheduleSpecRecord};
use crate::SchedulerContext;

/// A fire to dispatch after listeners are bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeIntentKind {
    ReclaimExisting,
    DispatchNew,
}

#[derive(Debug, Clone)]
pub struct ResumeIntent {
    pub fire_id: String,
    pub spec: ScheduleSpecRecord,
    pub scheduled_at: i64,
    pub trigger_reason: &'static str,
    pub kind: ResumeIntentKind,
}

/// Full reconciliation sequence. Run after `SchedulerDb` is open, before timer starts.
#[tracing::instrument(name = "scheduler:reconcile", skip(ctx))]
pub async fn reconcile<Ctx: SchedulerContext>(ctx: &Ctx) -> Result<Vec<ResumeIntent>> {
    tracing::info!("scheduler: starting reconciliation");

    // Step 1: Rebuild projection from CAS
    let schedules_dir = ctx
        .app_root()
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("schedules");
    let fires_dir = ctx
        .app_root()
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
    let rebuilt_specs = db.list_specs(false, None)?;
    db.rebuild_cursors_for_specs(&rebuilt_specs, lillux::time::timestamp_millis())?;
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
        let last_fire = db.get_last_fire(&spec.schedule_id)?;
        let plan = planning::plan_schedule(spec, last_fire.as_ref(), now);
        let horizon = plan.misfire_horizon_exclusive.unwrap_or(now);
        let pending = misfire::evaluate_misfires_before(spec, ctx, horizon, now).await;
        for p in pending {
            intents.push(ResumeIntent {
                fire_id: p.fire_id,
                spec: spec.clone(),
                scheduled_at: p.scheduled_at,
                trigger_reason: p.reason,
                kind: ResumeIntentKind::DispatchNew,
            });
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
                            let result_outcome = match ctx.get_thread_result_outcome(thread_id) {
                                Ok(outcome) => outcome,
                                Err(e) => {
                                    tracing::warn!(
                                        fire_id = %fire.fire_id,
                                        thread_id = %thread_id,
                                        error = %e,
                                        "recovered: failed to inspect completed thread result"
                                    );
                                    None
                                }
                            };
                            let outcome = super::completed_fire_outcome(result_outcome.as_ref());
                            update_fire_completed(ctx, fire, thread_id, "completed", outcome)
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
                                    kind: ResumeIntentKind::ReclaimExisting,
                                });
                            }
                            _ => {
                                update_fire_terminal(ctx, fire, None, "failed", "thread_lost")
                                    .await?;
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
                            kind: ResumeIntentKind::ReclaimExisting,
                        });
                    }
                    _ => {
                        update_fire_terminal(
                            ctx,
                            fire,
                            None,
                            "skipped",
                            "recovery_schedule_removed",
                        )
                        .await?;
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
    update_fire_terminal(ctx, fire, Some(thread_id), status, outcome).await
}

async fn update_fire_terminal<Ctx: SchedulerContext>(
    ctx: &Ctx,
    fire: &FireRecord,
    thread_id: Option<&str>,
    status: &str,
    outcome: &str,
) -> Result<()> {
    let now = lillux::time::timestamp_millis();

    let rec = FireRecord {
        status: status.to_string(),
        outcome: Some(outcome.to_string()),
        fired_at: Some(fire.fired_at.unwrap_or(now)),
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
        "trigger_reason": fire.trigger_reason,
        "outcome": outcome,
        "signer_fingerprint": fire.signer_fingerprint,
    });
    let fires_path = ctx
        .app_root()
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join("schedules")
        .join(&fire.schedule_id)
        .join("fires.jsonl");

    let db = ctx.scheduler_db();
    tokio::task::spawn_blocking(move || -> Result<()> {
        projection::append_jsonl_entry(&fires_path, &entry)?;
        db.upsert_fire(&rec)?;
        Ok(())
    })
    .await
    .map_err(|e| anyhow::anyhow!("spawn_blocking join error: {}", e))??;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ThreadResultOutcome;
    use ryeos_engine::trust::TrustStore;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;
    use tokio::sync::RwLock;

    struct MockContext {
        app_root: TempDir,
        db: Arc<crate::db::SchedulerDb>,
        gate: Arc<RwLock<()>>,
        trust: TrustStore,
        statuses: Mutex<HashMap<String, String>>,
        outcomes: Mutex<HashMap<String, ThreadResultOutcome>>,
    }

    impl MockContext {
        fn new() -> Self {
            Self {
                app_root: tempfile::tempdir().unwrap(),
                db: Arc::new(crate::db::SchedulerDb::new_in_memory().unwrap()),
                gate: Arc::new(RwLock::new(())),
                trust: TrustStore::empty(),
                statuses: Mutex::new(HashMap::new()),
                outcomes: Mutex::new(HashMap::new()),
            }
        }
    }

    impl SchedulerContext for MockContext {
        fn app_root(&self) -> &std::path::Path {
            self.app_root.path()
        }

        fn scheduler_db(&self) -> Arc<crate::db::SchedulerDb> {
            self.db.clone()
        }

        fn scheduler_runtime_gate(&self) -> Arc<RwLock<()>> {
            self.gate.clone()
        }

        fn trust_store(&self) -> &TrustStore {
            &self.trust
        }

        fn get_thread_status(&self, thread_id: &str) -> anyhow::Result<Option<String>> {
            Ok(self.statuses.lock().unwrap().get(thread_id).cloned())
        }

        fn get_thread_result_outcome(
            &self,
            thread_id: &str,
        ) -> anyhow::Result<Option<ThreadResultOutcome>> {
            Ok(self.outcomes.lock().unwrap().get(thread_id).cloned())
        }

        fn submit_cancel(&self, _thread_id: &str) -> anyhow::Result<()> {
            Ok(())
        }

        async fn dispatch_scheduled_item(
            &self,
            _spec: &ScheduleSpecRecord,
            _fire_id: &str,
            _thread_id: &str,
            _scheduled_at: i64,
            _trigger_reason: &str,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn recover_completed_fire_uses_result_failed_outcome() {
        let ctx = MockContext::new();
        let fire = FireRecord {
            fire_id: "sched@1000".to_string(),
            schedule_id: "sched".to_string(),
            scheduled_at: 1000,
            fired_at: Some(1001),
            completed_at: None,
            thread_id: Some("T-test".to_string()),
            status: "dispatched".to_string(),
            trigger_reason: "normal".to_string(),
            outcome: None,
            signer_fingerprint: Some("fp".to_string()),
        };
        ctx.db.upsert_fire(&fire).unwrap();
        ctx.statuses
            .lock()
            .unwrap()
            .insert("T-test".to_string(), "completed".to_string());
        ctx.outcomes.lock().unwrap().insert(
            "T-test".to_string(),
            ThreadResultOutcome::ResultFailed {
                reason: Some("boom".to_string()),
            },
        );

        let intents = recover_inflight_fires(&ctx).await.unwrap();

        assert!(intents.is_empty());
        let updated = ctx.db.get_fire("sched@1000").unwrap().unwrap();
        assert_eq!(updated.status, "completed");
        assert_eq!(updated.outcome.as_deref(), Some("result_failed"));
    }
}
