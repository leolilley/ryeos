//! Timer loop — the core scheduler.
//!
//! A `tokio::spawn` background task that sleeps until the next fire,
//! dispatches work, and listens for completion events.
//!
//! Dispatch is DETACHED: `dispatch_fire` claims the fire and records it
//! `dispatched` synchronously, then spawns the actual item execution
//! and returns. The runtime gate's read guard therefore covers only
//! planning + claim, never job execution — a long-running scheduled
//! job must not stall other schedules' evaluation, `scheduler/register`
//! (write side of the gate), or project apply-snapshot reconciliation.
//! Completion is finalized by the completion hook or the 30s repair
//! sweep, exactly as for any other in-flight fire.

use std::sync::Arc;

use anyhow::Context as _;
use tokio::sync::mpsc;

use super::misfire;
use super::overlap;
use super::planning;
use super::projection;
use super::types::{self, FireRecord, PendingFire, ReloadSignal, ScheduleSpecRecord};
use crate::SchedulerContext;

/// Start the timer loop. Call after listeners are bound.
pub async fn run<Ctx, Shutdown>(
    ctx: Arc<Ctx>,
    mut reload: mpsc::Receiver<ReloadSignal>,
    shutdown: Shutdown,
) -> anyhow::Result<()>
where
    Ctx: SchedulerContext,
    Shutdown: std::future::Future<Output = ()>,
{
    tokio::pin!(shutdown);
    let mut specs = Vec::new();
    let mut last_repair = lillux::time::timestamp_millis();

    tracing::info!(count = specs.len(), "scheduler: timer loop started");

    loop {
        let sleep_duration = {
            let runtime_gate = ctx.scheduler_runtime_gate();
            let Ok(_runtime_guard) = runtime_gate.try_read_owned() else {
                tracing::debug!("scheduler: runtime mutation in progress; timer cycle paused");
                tokio::select! {
                    _ = &mut shutdown => return Ok(()),
                    _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
                    Some(_) = reload.recv() => {}
                }
                continue;
            };

            if !ctx
                .scheduler_db()
                .fire_projection_is_current()
                .context("scheduler fire projection validity check failed")?
            {
                anyhow::bail!("scheduler fire projection became incomplete after daemon readiness");
            }

            specs = load_specs(&ctx)?;
            let now = lillux::time::timestamp_millis();

            // Periodic repair sweep: every 30 seconds, finalize stale dispatched fires
            if now - last_repair >= 30_000 {
                repair_stale_fires(&ctx).await?;
                last_repair = now;
            }

            // Collect all specs that are due right now using the same planner
            // consumed by diagnostics and recovery.
            let mut due_specs = Vec::new();
            for spec in &specs {
                if spec_is_due(spec, &ctx, now)? {
                    due_specs.push(spec.clone());
                }
            }

            // Process each due spec sequentially (ordering matters for overlap)
            for spec in &due_specs {
                process_spec(spec, &ctx).await?;
            }

            // Reload for next iteration
            specs = load_specs(&ctx)?;

            let now = lillux::time::timestamp_millis();
            let next_fire = compute_soonest_fire(&specs, &ctx, now)?;

            match next_fire {
                Some(at) if at > now => std::time::Duration::from_millis((at - now) as u64),
                _ => std::time::Duration::from_secs(1),
            }
        };

        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            _ = tokio::time::sleep(sleep_duration) => {}
            Some(_) = reload.recv() => {
                tracing::debug!("scheduler: reload requested");
                continue;
            }
        }
    }
}

/// Repair sweep: finalize dispatched fires whose threads have completed.
/// Catches cases where the finalize hook failed or the daemon crashed.
async fn repair_stale_fires<Ctx: SchedulerContext>(ctx: &Ctx) -> anyhow::Result<()> {
    let db = ctx.scheduler_db();
    let stale = db
        .find_stale_dispatched_fires(30)
        .context("scheduler repair sweep failed to query stale fires")?;

    for fire in &stale {
        fire.validate()
            .with_context(|| format!("invalid stale scheduler fire {}", fire.fire_id))?;
        if let Some(ref thread_id) = fire.thread_id {
            match ctx
                .get_thread_status(thread_id)
                .with_context(|| format!("inspect thread {thread_id} for stale scheduler fire"))?
            {
                Some(status) => {
                    if crate::thread_status_is_terminal(&status) {
                        let fire_status = crate::fire_status_for_thread_status(&status);
                        let result_outcome = if fire_status == "completed" {
                            ctx.get_thread_result_outcome(thread_id).with_context(|| {
                                format!(
                                    "inspect completed thread result for scheduler fire {}",
                                    fire.fire_id
                                )
                            })?
                        } else {
                            None
                        };
                        let outcome = crate::fire_outcome_for_terminal(
                            &status,
                            result_outcome.as_ref(),
                            None,
                        );
                        let now = lillux::time::timestamp_millis();
                        let rec = FireRecord {
                            status: fire_status.to_string(),
                            outcome: Some(outcome.clone()),
                            completed_at: Some(now),
                            ..fire.clone()
                        };
                        let app_root = ctx.app_root().to_path_buf();
                        let db = ctx.scheduler_db();
                        let fire_id_owned = fire.fire_id.clone();
                        tokio::task::spawn_blocking(move || {
                            projection::persist_fire_snapshot(&app_root, &db, &rec).with_context(
                                || format!("finalize stale scheduler fire {fire_id_owned}"),
                            )
                        })
                        .await
                        .context("scheduler repair persistence task stopped")??;
                        tracing::info!(
                            fire_id = %fire.fire_id,
                            thread_id = %thread_id,
                            thread_status = %status,
                            fire_status,
                            fire_outcome = %outcome,
                            "scheduler: repair sweep finalized stale fire"
                        );
                    }
                }
                None => {
                    // Thread gone — mark as failed
                    let now = lillux::time::timestamp_millis();
                    let rec = FireRecord {
                        status: "failed".to_string(),
                        outcome: Some("thread_lost".to_string()),
                        completed_at: Some(now),
                        ..fire.clone()
                    };
                    let app_root = ctx.app_root().to_path_buf();
                    let db = ctx.scheduler_db();
                    let fire_id_owned = fire.fire_id.clone();
                    tokio::task::spawn_blocking(move || {
                        projection::persist_fire_snapshot(&app_root, &db, &rec).with_context(|| {
                            format!("classify lost thread for scheduler fire {fire_id_owned}")
                        })
                    })
                    .await
                    .context("scheduler lost-thread persistence task stopped")??;
                }
            }
        }
    }
    Ok(())
}

fn load_specs<Ctx: SchedulerContext>(ctx: &Arc<Ctx>) -> anyhow::Result<Vec<ScheduleSpecRecord>> {
    let specs = ctx
        .scheduler_db()
        .load_enabled_specs()
        .context("scheduler failed to load enabled specs")?;
    for spec in &specs {
        types::validate_schedule_spec_record(spec).with_context(|| {
            format!(
                "scheduler projection contains invalid spec {}",
                spec.schedule_id
            )
        })?;
    }
    Ok(specs)
}

/// Check if a spec is due using the shared planner.
fn spec_is_due<Ctx: SchedulerContext>(
    spec: &ScheduleSpecRecord,
    ctx: &Ctx,
    now: i64,
) -> anyhow::Result<bool> {
    let db = ctx.scheduler_db();
    let last_fire = db
        .get_last_fire(&spec.schedule_id)
        .with_context(|| format!("query last fire for schedule {}", spec.schedule_id))?;
    if let Some(fire) = &last_fire {
        fire.validate()
            .with_context(|| format!("invalid last fire for schedule {}", spec.schedule_id))?;
    }

    Ok(planning::plan_schedule(spec, last_fire.as_ref(), now)
        .current_due_at
        .is_some())
}

fn compute_soonest_fire<Ctx: SchedulerContext>(
    specs: &[ScheduleSpecRecord],
    ctx: &Ctx,
    now: i64,
) -> anyhow::Result<Option<i64>> {
    let db = ctx.scheduler_db();
    let mut soonest = None;
    for spec in specs {
        let last_fire = db
            .get_last_fire(&spec.schedule_id)
            .with_context(|| format!("query next fire boundary for {}", spec.schedule_id))?;
        if let Some(fire) = &last_fire {
            fire.validate()
                .with_context(|| format!("invalid last fire for schedule {}", spec.schedule_id))?;
        }
        let plan = planning::plan_schedule(spec, last_fire.as_ref(), now);
        if let Some(candidate) = plan.current_due_at.or(plan.next_fire_at) {
            soonest = Some(soonest.map_or(candidate, |current: i64| current.min(candidate)));
        }
    }
    Ok(soonest)
}

async fn process_spec<Ctx: SchedulerContext>(
    spec: &ScheduleSpecRecord,
    ctx: &Arc<Ctx>,
) -> anyhow::Result<()> {
    let now = lillux::time::timestamp_millis();

    // Get last fire for this schedule to compute scheduled_at
    let db = ctx.scheduler_db();
    let last_fire = db
        .get_last_fire(&spec.schedule_id)
        .with_context(|| format!("query dispatch boundary for schedule {}", spec.schedule_id))?;
    if let Some(fire) = &last_fire {
        fire.validate()
            .with_context(|| format!("invalid dispatch boundary for {}", spec.schedule_id))?;
    }

    let plan = planning::plan_schedule(spec, last_fire.as_ref(), now);
    let scheduled_at = match plan.current_due_at {
        Some(at) => at,
        None => return Ok(()),
    };

    let fid = types::fire_id(&spec.schedule_id, scheduled_at);

    // ── Misfire evaluation ───────────────────────────────────
    // Only dispatch the current due boundary normally while it is within
    // grace. Once it is beyond grace, include it in misfire policy
    // evaluation instead of also dispatching it as a normal fire.
    let current_within_grace = plan.current_due_within_grace.unwrap_or(false);
    let misfire_horizon = plan.misfire_horizon_exclusive.unwrap_or(now);
    let misfire_fires =
        misfire::evaluate_misfires_before(spec, ctx.as_ref(), misfire_horizon, now).await?;

    // ── Build one ordered fire list (chronological) ──────────
    // Each fire gets its own overlap check before dispatch.
    let mut all_fires: Vec<PendingFire> = misfire_fires;
    if current_within_grace {
        all_fires.push(PendingFire {
            fire_id: fid,
            scheduled_at,
            reason: "normal",
        });
    }
    all_fires.sort_by_key(|f| f.scheduled_at);

    // ── Process each fire with overlap check ─────────────────
    for pending in all_fires {
        let overlap_ok = overlap::check_overlap(spec, ctx.as_ref()).await?;
        if !overlap_ok {
            // Skip this fire and all subsequent fires
            record_skip(
                ctx.as_ref(),
                &pending.fire_id,
                spec,
                pending.scheduled_at,
                "overlap_policy_skip",
            )
            .await?;
            break; // stop processing this batch, later fires reappear next tick
        }
        dispatch_fire(
            ctx,
            &pending.fire_id,
            spec,
            pending.scheduled_at,
            pending.reason,
            false,
        )
        .await
        .with_context(|| format!("admit scheduler fire {}", pending.fire_id))?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FireDispatchOutcome {
    Enqueued,
    AlreadyClaimed,
    AlreadyClassified,
    ClassifiedFailure,
}

pub async fn dispatch_fire<Ctx: SchedulerContext>(
    ctx: &Arc<Ctx>,
    fire_id: &str,
    spec: &ScheduleSpecRecord,
    scheduled_at: i64,
    trigger_reason: &str,
    skip_claim: bool,
) -> anyhow::Result<FireDispatchOutcome> {
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
            signer_fingerprint: spec.signer_fingerprint.clone(),
        };
        let db = ctx.scheduler_db();
        let app_root = ctx.app_root().to_path_buf();
        let fire_id_owned = fire_id.to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            projection::persist_fire_snapshot(&app_root, &db, &fail_rec).with_context(|| {
                format!("persist empty-capability failure for fire {fire_id_owned}")
            })?;
            Ok(())
        })
        .await
        .context("empty-capability failure persistence task stopped")??;
        return Ok(FireDispatchOutcome::ClassifiedFailure);
    }

    let thread_id = types::thread_id_from_fire(fire_id);
    let recovery_record = if skip_claim {
        Some(
            ctx.scheduler_db()
                .get_fire(fire_id)?
                .with_context(|| format!("recovery fire {fire_id} disappeared"))?,
        )
    } else {
        None
    };
    if recovery_record
        .as_ref()
        .and_then(|record| record.thread_id.as_deref())
        .is_some_and(|persisted| persisted != thread_id.as_str())
    {
        anyhow::bail!("recovery fire {fire_id} has non-deterministic thread identity");
    }
    let dispatched_at = recovery_record
        .as_ref()
        .and_then(|record| record.fired_at)
        .unwrap_or_else(lillux::time::timestamp_millis);
    let trigger_reason = recovery_record.as_ref().map_or_else(
        || trigger_reason.to_owned(),
        |record| record.trigger_reason.clone(),
    );

    // ── Record dispatched through DB + durable outbox ─────────
    let app_root = ctx.app_root().to_path_buf();
    let rec = FireRecord {
        fire_id: fire_id.to_string(),
        schedule_id: spec.schedule_id.clone(),
        scheduled_at,
        fired_at: Some(dispatched_at),
        completed_at: None,
        thread_id: Some(thread_id.clone()),
        status: "dispatched".to_string(),
        trigger_reason: trigger_reason.clone(),
        outcome: None,
        signer_fingerprint: spec.signer_fingerprint.clone(),
    };

    if !skip_claim {
        let app_root = app_root.clone();
        let db = ctx.scheduler_db();
        let fire_id_owned = fire_id.to_string();
        let claimed = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
            // Atomic claim: only proceed if this fire_id doesn't exist yet.
            // This prevents duplicate dispatch when recovery and timer race.
            match projection::claim_fire_snapshot(&app_root, &db, &rec)? {
                true => {
                    Ok(true)
                }
                false => {
                    tracing::debug!(fire_id = %fire_id_owned, "fire already claimed — skipping dispatch");
                    Ok(false)
                }
            }
        })
        .await
        .context("scheduler dispatch-claim task stopped")??;

        if !claimed {
            return Ok(FireDispatchOutcome::AlreadyClaimed);
        }
    } else {
        // Recovery redispatch uses the already-durable immutable dispatch
        // snapshot; it does not mint a second dispatch identity.
        rec.validate()?;
    }

    // ── Dispatch via SchedulerContext (DETACHED) ─────────────
    // The fire is claimed and recorded `dispatched`; the execution
    // itself runs on its own task so the timer loop (and the runtime
    // gate's read guard held by the caller) is released immediately.
    // A long-running scheduled job must never stall evaluation of
    // other schedules or block gate writers (scheduler/register,
    // project apply-snapshot). Success/failure of the dispatch is
    // recorded by the spawned task; completion of the thread itself is
    // finalized by the completion hook or the repair sweep.
    let ctx = Arc::clone(ctx);
    let spec = spec.clone();
    let fire_id = fire_id.to_string();
    tokio::spawn(async move {
        if !ctx.wait_for_recovery_execution_release().await {
            tracing::info!(
                fire_id = %fire_id,
                thread_id = %thread_id,
                "abandoned recovered scheduler dispatch after startup cancellation"
            );
            return;
        }
        match ctx
            .dispatch_scheduled_item(&spec, &fire_id, &thread_id, scheduled_at, &trigger_reason)
            .await
        {
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
                let completed_at = lillux::time::timestamp_millis();
                let fail_rec = FireRecord {
                    fire_id: fire_id.to_string(),
                    schedule_id: spec.schedule_id.clone(),
                    scheduled_at,
                    fired_at: Some(dispatched_at),
                    completed_at: Some(completed_at),
                    thread_id: Some(thread_id),
                    status: "failed".to_string(),
                    trigger_reason: trigger_reason.to_string(),
                    outcome: Some("dispatch_failed".to_string()),
                    signer_fingerprint: spec.signer_fingerprint.clone(),
                };
                {
                    let _runtime_guard = ctx.scheduler_runtime_gate().read_owned().await;
                    let db = ctx.scheduler_db();
                    let app_root = ctx.app_root().to_path_buf();
                    let fire_id_log = fire_id.clone();
                    tokio::task::spawn_blocking(move || {
                        if let Err(e) =
                            projection::persist_fire_snapshot(&app_root, &db, &fail_rec)
                        {
                            tracing::error!(fire_id = %fail_rec.fire_id, error = %e,
                                "dispatch failure: failed to upsert fire record");
                        }
                    }).await.unwrap_or_else(|e| {
                        tracing::error!(fire_id = %fire_id_log, error = %e, "spawn_blocking task panicked or was cancelled (failure persist)");
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
    });
    Ok(FireDispatchOutcome::Enqueued)
}

/// Dispatch a recovery fire (from reconciler).
/// Uses reclaim path instead of claim to handle fires already persisted in DB.
pub async fn dispatch_recovery_fire<Ctx: SchedulerContext>(
    ctx: Arc<Ctx>,
    intent: super::reconcile::ResumeIntent,
) -> anyhow::Result<FireDispatchOutcome> {
    let _runtime_guard = ctx.scheduler_runtime_gate().read_owned().await;

    dispatch_recovery_fire_inner(ctx, intent).await
}

/// Persist and enqueue one recovery fire while the daemon still owns the
/// scheduler startup gate's exclusive side. Requiring the owned write guard by
/// reference makes the pre-Ready call site explicit and avoids attempting to
/// reacquire this same gate through [`dispatch_recovery_fire`].
pub async fn dispatch_recovery_fire_under_startup_guard<Ctx: SchedulerContext>(
    ctx: Arc<Ctx>,
    intent: super::reconcile::ResumeIntent,
    _startup_guard: &tokio::sync::OwnedRwLockWriteGuard<()>,
) -> anyhow::Result<FireDispatchOutcome> {
    dispatch_recovery_fire_inner(ctx, intent).await
}

async fn dispatch_recovery_fire_inner<Ctx: SchedulerContext>(
    ctx: Arc<Ctx>,
    intent: super::reconcile::ResumeIntent,
) -> anyhow::Result<FireDispatchOutcome> {
    if matches!(intent.kind, super::reconcile::ResumeIntentKind::DispatchNew) {
        return dispatch_fire(
            &ctx,
            &intent.fire_id,
            &intent.spec,
            intent.scheduled_at,
            intent.trigger_reason,
            false,
        )
        .await;
    }

    // Attempt to reclaim the fire for redispatch.
    // This handles the case where fire was persisted but thread was never created.
    let fire_id = intent.fire_id.clone();
    let db = ctx.scheduler_db();
    let app_root = ctx.app_root().to_path_buf();
    let can_reclaim = tokio::task::spawn_blocking(move || {
        projection::reclaim_fire_snapshot(&app_root, &db, &fire_id)
    })
    .await
    .context("scheduler fire-reclaim task stopped")??;

    if !can_reclaim {
        let record = ctx.scheduler_db().get_fire(&intent.fire_id)?;
        if record.as_ref().is_some_and(|record| {
            matches!(
                record.status.as_str(),
                "completed" | "failed" | "cancelled" | "skipped"
            )
        }) {
            return Ok(FireDispatchOutcome::AlreadyClassified);
        }
        anyhow::bail!(
            "recovered fire {} could neither be reclaimed nor shown terminal",
            intent.fire_id
        );
    }

    dispatch_fire(
        &ctx,
        &intent.fire_id,
        &intent.spec,
        intent.scheduled_at,
        intent.trigger_reason,
        true,
    )
    .await
}

async fn record_skip<Ctx: SchedulerContext>(
    ctx: &Ctx,
    fire_id: &str,
    spec: &ScheduleSpecRecord,
    scheduled_at: i64,
    reason: &str,
) -> anyhow::Result<()> {
    let now = lillux::time::timestamp_millis();

    let app_root = ctx.app_root().to_path_buf();
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
    {
        let db = ctx.scheduler_db();
        let fire_id_owned = fire_id.to_string();
        tokio::task::spawn_blocking(move || {
            projection::persist_fire_snapshot(&app_root, &db, &rec)
                .with_context(|| format!("persist skipped scheduler fire {fire_id_owned}"))
        })
        .await
        .context("scheduler skip persistence task stopped")??;
    }
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
        /// When true, `dispatch_scheduled_item` signals `dispatch_started`
        /// then parks on `dispatch_release` — simulating a long-running
        /// scheduled job for detachment tests.
        hang_dispatch: std::sync::atomic::AtomicBool,
        dispatch_started: Arc<tokio::sync::Notify>,
        dispatch_release: Arc<tokio::sync::Notify>,
    }

    impl MockContext {
        fn new() -> Self {
            let app_root = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(app_root.path().join(ryeos_engine::AI_DIR).join("state"))
                .unwrap();
            Self {
                app_root,
                db: Arc::new(crate::db::SchedulerDb::new_in_memory().unwrap()),
                gate: Arc::new(RwLock::new(())),
                trust: TrustStore::empty(),
                statuses: Mutex::new(HashMap::new()),
                outcomes: Mutex::new(HashMap::new()),
                hang_dispatch: std::sync::atomic::AtomicBool::new(false),
                dispatch_started: Arc::new(tokio::sync::Notify::new()),
                dispatch_release: Arc::new(tokio::sync::Notify::new()),
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
            if self.hang_dispatch.load(std::sync::atomic::Ordering::SeqCst) {
                self.dispatch_started.notify_one();
                self.dispatch_release.notified().await;
            }
            Ok(())
        }
    }

    fn make_spec(schedule_id: &str, overlap_policy: &str) -> ScheduleSpecRecord {
        ScheduleSpecRecord {
            schedule_id: schedule_id.to_string(),
            item_ref: "directive:test/job".to_string(),
            ref_bindings: std::collections::BTreeMap::new(),
            params: "{}".to_string(),
            schedule_type: "cron".to_string(),
            expression: "* * * * * *".to_string(),
            timezone: "UTC".to_string(),
            misfire_policy: "skip".to_string(),
            overlap_policy: overlap_policy.to_string(),
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

    #[tokio::test]
    async fn invalid_fire_projection_terminates_timer() {
        let ctx = Arc::new(MockContext::new());
        ctx.db.begin_fire_projection_rebuild().unwrap();
        let (_reload_tx, reload_rx) = tokio::sync::mpsc::channel(1);

        let error = run(ctx, reload_rx, std::future::pending::<()>())
            .await
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("fire projection became incomplete"));
    }

    /// A long-running scheduled job must not hold the timer or the
    /// runtime gate: `dispatch_fire` returns once the fire is claimed,
    /// the fire row is `dispatched`, and a gate WRITER (scheduler/register,
    /// project apply-snapshot) can proceed while the job is still running.
    #[tokio::test]
    async fn dispatch_fire_is_detached_and_releases_gate() {
        let ctx = Arc::new(MockContext::new());
        ctx.hang_dispatch
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let spec = make_spec("long-job", "skip");
        let scheduled_at = 1000;
        let fire_id = types::fire_id(&spec.schedule_id, scheduled_at);

        // Mirror the timer loop: dispatch while holding the gate's read guard.
        let gate = ctx.scheduler_runtime_gate();
        {
            let _read = gate.clone().try_read_owned().expect("gate free");
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                dispatch_fire(&ctx, &fire_id, &spec, scheduled_at, "normal", false),
            )
            .await
            .expect("dispatch_fire must return while the job is still running")
            .expect("fire admission must succeed");
        }

        // The job actually started (and is parked).
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            ctx.dispatch_started.notified(),
        )
        .await
        .expect("spawned dispatch must start");

        // Fire is recorded dispatched.
        let fire = ctx.db.get_fire(&fire_id).unwrap().expect("fire claimed");
        assert_eq!(fire.status, "dispatched");

        // A gate writer must not block behind the running job. We only
        // assert the writer *acquires* within the timeout; the guard itself
        // is unused and released immediately.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), gate.write())
            .await
            .expect("gate writer must acquire while a scheduled job is running");

        ctx.dispatch_release.notify_one();
    }

    /// Detached dispatch means a claimed fire's thread row may not exist
    /// yet at the next overlap evaluation. Skip/cancel_previous must
    /// treat that as still-pending, not proceed into a double-run.
    #[tokio::test]
    async fn overlap_holds_when_inflight_thread_not_yet_visible() {
        let ctx = MockContext::new();
        for policy in ["skip", "cancel_previous"] {
            let spec = make_spec(&format!("race-{policy}"), policy);
            let fire_id = types::fire_id(&spec.schedule_id, 1000);
            let thread_id = types::thread_id_from_fire(&fire_id);
            let fire = FireRecord {
                fire_id,
                schedule_id: spec.schedule_id.clone(),
                scheduled_at: 1000,
                fired_at: Some(lillux::time::timestamp_millis()),
                completed_at: None,
                thread_id: Some(thread_id),
                status: "dispatched".to_string(),
                trigger_reason: "normal".to_string(),
                outcome: None,
                signer_fingerprint: "11".repeat(32),
            };
            ctx.db.upsert_fire(&fire).unwrap();
            // No status entry for the deterministic thread → get_thread_status None.
            assert!(
                !overlap::check_overlap(&spec, &ctx).await.unwrap(),
                "policy {policy}: in-flight fire with invisible thread must hold the boundary"
            );
        }
    }

    #[tokio::test]
    async fn repair_stale_completed_fire_uses_result_failed_outcome() {
        let ctx = MockContext::new();
        let thread_id = types::thread_id_from_fire("sched@1000");
        let fire = FireRecord {
            fire_id: "sched@1000".to_string(),
            schedule_id: "sched".to_string(),
            scheduled_at: 1000,
            fired_at: Some(lillux::time::timestamp_millis() - 60_000),
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

        repair_stale_fires(&ctx).await.unwrap();

        let updated = ctx.db.get_fire("sched@1000").unwrap().unwrap();
        assert_eq!(updated.status, "completed");
        assert_eq!(updated.outcome.as_deref(), Some("result_failed"));

        let jsonl = std::fs::read_to_string(
            ctx.app_root
                .path()
                .join(ryeos_engine::AI_DIR)
                .join("state")
                .join("schedules")
                .join("sched")
                .join("fires.jsonl"),
        )
        .unwrap();
        assert!(jsonl.contains(r#""outcome":"result_failed""#));
    }
}
