//! Scheduler reconciliation — crash recovery.
//!
//! Follows the existing thread reconciler pattern:
//! 1. Synchronize signed schedule specs and validate persisted scheduler state
//! 2. Recover in-flight fires (check thread status)
//! 3. Evaluate misfire gaps
//! 4. Collect intents to dispatch after listeners are bound
//!
//! A normal restart with a current projection never replays historical fire
//! JSONL. Durable SQLite supplies indexed in-flight rows and scheduling
//! cursors. Full journal replay is reserved for a fresh or explicitly
//! invalidated projection.

use anyhow::{Context, Result};

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

    // Step 1: synchronize the small signed-spec set. This is proportional to
    // live schedules, not retained fire history.
    let schedules_dir = ctx
        .app_root()
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("schedules");
    let runtime_state_dir = ctx.app_root().join(ryeos_engine::AI_DIR).join("state");
    let fires_dir = runtime_state_dir.join("schedules");

    let db = ctx.scheduler_db();
    let drained = db.drain_fire_outbox(&runtime_state_dir)?;
    if drained > 0 {
        tracing::info!(drained, "scheduler: recovered durable fire-journal outbox");
    }
    let live_ids =
        projection::rebuild_specs_from_dir(&schedules_dir, &db, ctx.schedule_trust_store())?;

    let specs = db.list_specs(false, None)?;
    let now = lillux::time::timestamp_millis();
    if db.fire_projection_is_current()? {
        let repaired = db.reconcile_cursors_for_specs(&specs, now)?;
        tracing::info!(
            specs = live_ids.len(),
            cursors_repaired = repaired,
            "scheduler: persisted projection reconciled incrementally"
        );
    } else {
        tracing::warn!(
            path = %fires_dir.display(),
            "scheduler: fire projection is fresh or invalid; replaying retained journals"
        );
        projection::rebuild_fires_from_dir(&fires_dir, &db)?;
        db.rebuild_cursors_for_specs(&specs, now)?;
        tracing::info!(
            specs = live_ids.len(),
            "scheduler: fire projection rebuilt from retained journals"
        );
    }

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
        let cursor = db.get_cursor(&spec.schedule_id)?.ok_or_else(|| {
            anyhow::anyhow!(
                "scheduler cursor missing after startup reconciliation for {}",
                spec.schedule_id
            )
        })?;
        if cursor.spec_hash != spec.spec_hash {
            anyhow::bail!(
                "scheduler cursor/spec mismatch after startup reconciliation for {}",
                spec.schedule_id
            );
        }
        let plan = planning::plan_schedule_from_cursor(spec, cursor.last_scheduled_at, now);
        let horizon = plan.misfire_horizon_exclusive.unwrap_or(now);
        let pending = misfire::evaluate_misfires_before(spec, ctx, horizon, now).await?;
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
                    Ok(Some(status)) if crate::thread_status_is_terminal(&status) => {
                        let result_outcome = if status == "completed" {
                            match ctx.get_thread_result_outcome(thread_id) {
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
                            }
                        } else {
                            None
                        };
                        let fire_status = crate::fire_status_for_thread_status(&status);
                        let outcome = crate::fire_outcome_for_terminal(
                            &status,
                            result_outcome.as_ref(),
                            None,
                        );
                        update_fire_completed(ctx, fire, thread_id, fire_status, &outcome).await?;
                        tracing::info!(
                            fire_id = %fire.fire_id,
                            thread_id = %thread_id,
                            thread_status = %status,
                            fire_status,
                            fire_outcome = %outcome,
                            "recovered: terminal thread classified during downtime"
                        );
                    }
                    Ok(Some(status)) => {
                        tracing::warn!(
                            fire_id = %fire.fire_id,
                            thread_id = %thread_id,
                            status = %status,
                            "recovered: thread in non-terminal status — existing reconciler handles"
                        );
                    }
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
                    Err(error) => return Err(error),
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
    if let Some(thread_id) = thread_id {
        if fire.thread_id.as_deref() != Some(thread_id) {
            anyhow::bail!("terminal fire update changed deterministic thread identity");
        }
    }
    let fired_at = fire.fired_at.context("dispatched fire has no fired_at")?;

    let rec = FireRecord {
        status: status.to_string(),
        outcome: Some(outcome.to_string()),
        fired_at: Some(fired_at),
        completed_at: Some(now.max(fired_at)),
        ..fire.clone()
    };

    let app_root = ctx.app_root().to_path_buf();

    let db = ctx.scheduler_db();
    tokio::task::spawn_blocking(move || -> Result<()> {
        projection::persist_fire_snapshot(&app_root, &db, &rec)?;
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

        fn schedule_trust_store(&self) -> &TrustStore {
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
        let thread_id = crate::types::thread_id_from_fire("sched@1000");
        let fire = FireRecord {
            fire_id: "sched@1000".to_string(),
            schedule_id: "sched".to_string(),
            scheduled_at: 1000,
            fired_at: Some(1001),
            completed_at: None,
            thread_id: Some(thread_id.clone()),
            status: "dispatched".to_string(),
            trigger_reason: "normal".to_string(),
            outcome: None,
            signer_fingerprint: "11".repeat(32),
        };
        ctx.db.upsert_fire(&fire).unwrap();
        ctx.statuses
            .lock()
            .unwrap()
            .insert(thread_id.clone(), "completed".to_string());
        ctx.outcomes.lock().unwrap().insert(
            thread_id,
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

    #[tokio::test]
    async fn current_projection_startup_does_not_replay_fire_jsonl() {
        let ctx = MockContext::new();
        ctx.db.begin_fire_projection_rebuild().unwrap();
        ctx.db.finish_fire_projection_rebuild().unwrap();

        let retained = FireRecord {
            fire_id: "persisted@1000".to_string(),
            schedule_id: "persisted".to_string(),
            scheduled_at: 1_000,
            fired_at: Some(1_001),
            completed_at: Some(1_002),
            thread_id: Some(crate::types::thread_id_from_fire("persisted@1000")),
            status: "completed".to_string(),
            trigger_reason: "normal".to_string(),
            outcome: Some("success".to_string()),
            signer_fingerprint: "11".repeat(32),
        };
        ctx.db.upsert_fire(&retained).unwrap();

        let journal_dir = ctx
            .app_root()
            .join(ryeos_engine::AI_DIR)
            .join("state")
            .join("schedules")
            .join("persisted");
        std::fs::create_dir_all(&journal_dir).unwrap();
        std::fs::write(
            journal_dir.join("fires.jsonl"),
            "{this would be ignored only by incremental startup}\n",
        )
        .unwrap();

        let intents = reconcile(&ctx).await.unwrap();
        assert!(intents.is_empty());
        assert!(ctx.db.get_fire("persisted@1000").unwrap().is_some());
    }
}
