//! `scheduler.deregister` — remove a schedule spec.
//!
//! Ownership check: callers can only deregister their own schedules.

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

    // Delete YAML file
    let yaml_path = state
        .config
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("schedules")
        .join(format!("{}.yaml", req.schedule_id));
    if yaml_path.exists() {
        std::fs::remove_file(&yaml_path).map_err(|e| HandlerError::Internal(e.to_string()))?;
    }

    // Delete projection rows
    state
        .scheduler_db
        .delete_spec(&req.schedule_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
    state
        .scheduler_db
        .delete_fires_for_schedule(&req.schedule_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?;

    // The on-disk JSONL history dir is preserved by default (audit trail), but
    // it blocks re-registering the same id. `purge_history` removes it so the id
    // can be recreated cleanly without hand-deleting files.
    let mut history_purged = false;
    if req.purge_history {
        let fires_dir = state
            .config
            .app_root
            .join(ryeos_engine::AI_DIR)
            .join("state")
            .join("schedules")
            .join(&req.schedule_id);
        if fires_dir.exists() {
            std::fs::remove_dir_all(&fires_dir)
                .map_err(|e| HandlerError::Internal(e.to_string()))?;
            history_purged = true;
        }
    }

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
