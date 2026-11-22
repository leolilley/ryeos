use anyhow::{Context, Result};
use ryeos_app::retention_exclusion::{
    ExclusionMode, PinFuture, PinSourceId, ThreadPinGuard, ThreadPinSource,
};
use ryeos_app::scheduler_boundary::{
    DurableThreadTerminal, SchedulerParticipant, SchedulerRetentionGuard,
    TerminalObservationDisposition,
};
use ryeos_scheduler::{
    classify_result_payload, fire_outcome_for_terminal, fire_status_for_thread_status, projection,
};
use ryeos_state::gc::retention::FireRetentionTarget;
use std::sync::Arc;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard};

use crate::retention::{SchedulerService, SchedulerServiceRetention};

impl SchedulerParticipant for SchedulerService {
    fn observe_terminal(
        &self,
        observation: &DurableThreadTerminal<'_>,
    ) -> Result<TerminalObservationDisposition> {
        let Ok(_gate) = self.runtime_gate.clone().try_read_owned() else {
            tracing::debug!(thread_id = %observation.thread_id(),
                "scheduler terminal observation deferred behind runtime mutation");
            return Ok(TerminalObservationDisposition::Deferred);
        };
        if self.require_dispatch_available().is_err() {
            tracing::debug!(thread_id = %observation.thread_id(),
                "scheduler terminal observation deferred during project schedule recovery");
            return Ok(TerminalObservationDisposition::Deferred);
        }
        let Some(fire) = self
            .db
            .find_fire_by_thread(observation.thread_id())
            .context("find scheduler fire for durable terminal thread")?
        else {
            return Ok(TerminalObservationDisposition::NotScheduled);
        };

        let fire_status = fire_status_for_thread_status(observation.terminal_status());
        let result_outcome = observation.result().map(classify_result_payload);
        let outcome = fire_outcome_for_terminal(
            observation.terminal_status(),
            result_outcome.as_ref(),
            observation.outcome_code(),
        );
        let now = lillux::time::timestamp_millis();

        // Thread completion can win the race against the asynchronous
        // scheduler handoff. Recover that exact authority only through the
        // lifecycle's admitted-capsule lookup and persist the intermediate
        // dispatched record before terminal status.
        let fire = if fire.status == "reserved" {
            let Some(capsule_hash) = observation
                .admitted_launch_capsule_hash()
                .context("read admitted launch capsule for reserved scheduler fire")?
            else {
                tracing::warn!(thread_id = %observation.thread_id(),
                    "reserved scheduler fire has no admitted launch capsule; defer repair");
                return Ok(TerminalObservationDisposition::Deferred);
            };
            if fire.project_authority.is_none() {
                tracing::warn!(thread_id = %observation.thread_id(),
                    "reserved scheduler fire has no immutable project authority; defer repair");
                return Ok(TerminalObservationDisposition::Deferred);
            }
            let dispatched = ryeos_scheduler::FireRecord {
                status: "dispatched".to_string(),
                dispatched_at: Some(now.max(fire.reserved_at)),
                admitted_capsule_hash: Some(capsule_hash),
                ..fire
            };
            projection::persist_fire_snapshot(&self.app_root, &self.db, &dispatched)
                .context("persist recovered scheduler dispatch handoff")?;
            dispatched
        } else {
            fire
        };
        if fire.status != "dispatched" {
            return Ok(TerminalObservationDisposition::NotScheduled);
        }
        let updated = ryeos_scheduler::FireRecord {
            status: fire_status.to_string(),
            outcome: Some(outcome.to_string()),
            completed_at: Some(now.max(fire.dispatched_at.unwrap_or(fire.reserved_at))),
            ..fire
        };
        projection::persist_fire_snapshot(&self.app_root, &self.db, &updated)
            .context("persist scheduler fire terminal outcome")?;
        Ok(TerminalObservationDisposition::Recorded)
    }

    fn pin_source(self: Arc<Self>) -> Arc<dyn ThreadPinSource> {
        self
    }

    fn acquire_retention(
        &self,
    ) -> ryeos_app::scheduler_boundary::SchedulerFuture<'_, Box<dyn SchedulerRetentionGuard>> {
        Box::pin(async move {
            let gate: OwnedRwLockWriteGuard<()> = self.runtime_gate.clone().write_owned().await;
            self.require_dispatch_available()?;
            if !self.db.fire_projection_is_current()? {
                anyhow::bail!(
                    "scheduler retention requires a current fire projection before pin inspection"
                );
            }
            Ok(Box::new(SchedulerServiceRetention::new(
                PinSourceId::of(self),
                gate,
                self.db.clone(),
                self.runtime_directory.clone(),
                self.runtime_identity,
            )) as Box<dyn SchedulerRetentionGuard>)
        })
    }
}

impl ThreadPinSource for SchedulerService {
    fn acquire_pins(&self, mode: ExclusionMode) -> PinFuture<'_, Box<dyn ThreadPinGuard>> {
        Box::pin(async move {
            match mode {
                ExclusionMode::Exclusive => {
                    let guard = SchedulerParticipant::acquire_retention(self).await?;
                    Ok(guard as Box<dyn ThreadPinGuard>)
                }
                ExclusionMode::Shared => {
                    let gate = self.runtime_gate.clone().read_owned().await;
                    self.require_dispatch_available()?;
                    if !self.db.fire_projection_is_current()? {
                        anyhow::bail!(
                            "scheduler retention requires a current fire projection before pin inspection"
                        );
                    }
                    Ok(Box::new(SchedulerSharedPins {
                        source: PinSourceId::of(self),
                        _gate: gate,
                        db: self.db.clone(),
                    }) as Box<dyn ThreadPinGuard>)
                }
            }
        })
    }
}

/// Read side of the scheduler runtime gate with a verified-current fire
/// projection, for work that only needs stable pin counts.
struct SchedulerSharedPins {
    source: PinSourceId,
    _gate: OwnedRwLockReadGuard<()>,
    db: Arc<ryeos_scheduler::db::SchedulerDb>,
}

fn count_fire_pins(db: &ryeos_scheduler::db::SchedulerDb, thread_ids: &[String]) -> Result<u64> {
    let mut pins = 0u64;
    for thread_id in thread_ids {
        if db.find_fire_by_thread(thread_id)?.is_some() {
            pins = pins
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("scheduler pin count overflow"))?;
        }
    }
    Ok(pins)
}

impl ThreadPinGuard for SchedulerSharedPins {
    fn source(&self) -> PinSourceId {
        self.source
    }
    fn count_thread_pins(&self, thread_ids: &[String]) -> Result<u64> {
        count_fire_pins(&self.db, thread_ids)
    }
}

impl ThreadPinGuard for SchedulerServiceRetention {
    fn source(&self) -> PinSourceId {
        self.source
    }
    fn count_thread_pins(&self, thread_ids: &[String]) -> Result<u64> {
        count_fire_pins(&self.db, thread_ids)
    }
}

impl SchedulerRetentionGuard for SchedulerServiceRetention {
    fn operational_snapshot_roots(&self) -> Result<Vec<String>> {
        self.db.operational_snapshot_roots()
    }

    fn prepare_fire_retention(
        &mut self,
        runtime: &lillux::PinnedDirectory,
        dry_run: bool,
    ) -> Result<()> {
        let identity = runtime.identity()?;
        if identity != self.runtime_identity {
            anyhow::bail!("scheduler retention runtime directory identity mismatch");
        }
        if self.prepared {
            anyhow::bail!("scheduler fire retention was already prepared");
        }
        if dry_run {
            let pending = self.db.pending_fire_outbox()?;
            if pending != 0 {
                anyhow::bail!(
                    "scheduler retention dry-run requires an empty fire outbox; {pending} snapshot(s) remain"
                );
            }
        } else {
            self.db
                .drain_fire_outbox_in_directory(runtime)
                .context("drain scheduler outbox before fire-journal retention")?;
            self.db
                .begin_fire_retention()
                .context("invalidate scheduler fire projection before journal retention")?;
            self.projection_invalidated = true;
        }
        self.dry_run = Some(dry_run);
        self.prepared = true;
        Ok(())
    }

    fn finish_fire_retention(&mut self, targets: &[FireRetentionTarget]) -> Result<usize> {
        let Some(dry_run) = self.dry_run else {
            anyhow::bail!("scheduler fire retention was not prepared");
        };
        if !self.prepared || self.finished {
            anyhow::bail!("scheduler fire retention is not in a repairable phase");
        }
        let rows = if dry_run {
            self.db.verify_fire_retention_targets(targets)?;
            targets.len()
        } else {
            self.db.finish_fire_retention(targets)?
        };
        self.finished = true;
        Ok(rows)
    }

    fn repair_fire_retention(&mut self) -> Result<()> {
        if !self.prepared || self.dry_run != Some(false) || !self.projection_invalidated {
            anyhow::bail!("scheduler fire retention has no invalidated projection to repair");
        }
        if self.finished {
            anyhow::bail!("scheduler fire retention repair was already completed");
        }
        projection::rebuild_fires_from_runtime_directory(&self.runtime_directory, &self.db)?;
        let specs = self.db.list_specs(false, None)?;
        self.db
            .rebuild_cursors_for_specs(&specs, lillux::time::timestamp_millis())?;
        self.finished = true;
        self.projection_invalidated = false;
        Ok(())
    }
}
