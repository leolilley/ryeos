//! `scheduler.deregister` — remove a schedule spec.
//!
//! Ownership check: callers can only deregister their own schedules.

use std::io::Read as _;
use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schedule_id: String,
    /// Also delete the on-disk fire-history dir (`.ai/state/schedules/<id>/`).
    /// Off by default (history is kept for audit), but the history dir blocks
    /// re-registering the same `schedule_id`, so set this when you intend to
    /// recreate the schedule cleanly instead of hand-deleting files.
    #[serde(default)]
    pub purge_history: bool,
}

pub async fn handle(
    req: Request,
    ctx: crate::handler_context::HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let _mutation_guard = state.scheduler_runtime_gate.clone().write_owned().await;
    ryeos_scheduler::crontab::validate_schedule_id(&req.schedule_id)
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;

    let spec = state
        .scheduler_db
        .get_spec(&req.schedule_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .ok_or(HandlerError::NotFound)?;

    ctx.require_owner(Some(&spec.requester_fingerprint))?;

    let yaml_path = super::scheduler_register::canonical_schedule_source_path(
        &state.config.app_root,
        &req.schedule_id,
    )
    .map_err(|e| HandlerError::Internal(e.to_string()))?;
    let schedules_path = yaml_path
        .parent()
        .ok_or_else(|| HandlerError::Internal("schedule source has no parent".to_string()))?;
    let schedules_directory = lillux::PinnedDirectory::open(schedules_path)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .ok_or_else(|| HandlerError::Internal("schedule source directory is absent".to_string()))?;
    let _source_lock = schedules_directory
        .lock_exclusive()
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
    let source_name = yaml_path
        .file_name()
        .ok_or_else(|| HandlerError::Internal("schedule source has no filename".to_string()))?;
    let source_file = schedules_directory
        .open_regular(source_name, false)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
    // When present, the exact source currently represented by SQLite must be
    // trusted before an authenticated mutation is allowed to remove it. A
    // missing source is accepted as recovery of a prior crash after the
    // durable unlink but before projection cleanup.
    if let Some(file) = source_file.as_ref() {
        let mut content = String::new();
        file.try_clone()
            .and_then(|mut file| file.read_to_string(&mut content))
            .map_err(|e| HandlerError::Internal(e.to_string()))?;
        let verified = ryeos_scheduler::projection::verify_schedule_source_content(
            &yaml_path,
            &content,
            &state.engine.trust_store,
        )
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
        if verified.spec_hash != spec.spec_hash {
            return Err(HandlerError::Internal(
                "schedule source changed since it was projected".to_string(),
            ));
        }
    }

    // Disable the projection first. If the process stops after the source is
    // removed but before the final DB delete, this stale row cannot dispatch.
    let mut disabled_spec = spec.clone();
    disabled_spec.enabled = false;
    state
        .scheduler_db
        .upsert_spec(&disabled_spec)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
    // Publish the fail-closed state before any fallible history work. The
    // timer may have cached the old enabled row, and must not dispatch it if a
    // purge fails after this point.
    if let Some(ref tx) = state.scheduler_reload_tx {
        if let Err(e) = tx.try_send(ryeos_scheduler::ReloadSignal {
            schedule_id: Some(req.schedule_id.clone()),
        }) {
            tracing::warn!(schedule_id = %req.schedule_id, error = %e, "scheduler reload channel full or closed after disable — timer will pick up changes on next tick");
        }
    }

    // The on-disk JSONL history dir is preserved by default (audit trail), but
    // it blocks re-registering the same id. `purge_history` removes it so the id
    // can be recreated cleanly without hand-deleting files.
    let mut history_purged = false;
    if req.purge_history {
        let runtime_state_dir = state
            .config
            .app_root
            .join(ryeos_engine::AI_DIR)
            .join("state");
        let fires_root = runtime_state_dir.join("schedules");
        let fires_dir = fires_root.join(&req.schedule_id);
        state
            .scheduler_db
            .drain_fire_outbox(&runtime_state_dir)
            .map_err(|e| HandlerError::Internal(e.to_string()))?;
        state
            .scheduler_db
            .begin_fire_retention()
            .map_err(|e| HandlerError::Internal(e.to_string()))?;
        let purge_result = (|| -> anyhow::Result<()> {
            match std::fs::symlink_metadata(&fires_root) {
                Ok(metadata)
                    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => anyhow::bail!(
                    "scheduler fires root is not a real directory: {}",
                    fires_root.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            match std::fs::symlink_metadata(&fires_dir) {
                Ok(metadata)
                    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() =>
                {
                    lillux::remove_dir_all_durable(&fires_dir)?;
                    history_purged = true;
                }
                Ok(_) => anyhow::bail!(
                    "scheduler history is not a real directory: {}",
                    fires_dir.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            state
                .scheduler_db
                .finish_schedule_fire_purge(&req.schedule_id)?;
            Ok(())
        })();
        if let Err(error) = purge_result {
            let repair = (|| -> anyhow::Result<()> {
                ryeos_scheduler::projection::rebuild_fires_from_dir(
                    &fires_root,
                    &state.scheduler_db,
                )?;
                let specs = state.scheduler_db.list_specs(false, None)?;
                state
                    .scheduler_db
                    .rebuild_cursors_for_specs(&specs, lillux::time::timestamp_millis())?;
                Ok(())
            })();
            return Err(HandlerError::Internal(match repair {
                Ok(()) => error.to_string(),
                Err(repair_error) => {
                    format!("{error:#}; scheduler fire projection repair failed: {repair_error:#}")
                }
            }));
        }
    }

    if let Some(file) = source_file.as_ref() {
        schedules_directory
            .remove_if_same(source_name, file)
            .map_err(|e| HandlerError::Internal(e.to_string()))?;
    }
    state
        .scheduler_db
        .delete_spec(&req.schedule_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    // Ping timer loop
    if let Some(ref tx) = state.scheduler_reload_tx {
        if let Err(e) = tx.try_send(ryeos_scheduler::ReloadSignal {
            schedule_id: Some(req.schedule_id.clone()),
        }) {
            tracing::warn!(schedule_id = %req.schedule_id, error = %e, "scheduler reload channel full or closed — timer will pick up changes on next tick");
        }
    }

    Ok(serde_json::json!({
        "schedule_id": req.schedule_id,
        "deleted": true,
        "history_preserved": !history_purged,
        "history_purged": history_purged,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:scheduler/deregister",
    endpoint: "scheduler.deregister",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.scheduler/deregister"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};
