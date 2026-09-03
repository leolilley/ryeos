//! `sync/jobs/list` — inspect durable distributed sync jobs.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

fn default_limit() -> usize {
    50
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let job_state = req.state.as_deref().map(parse_sync_job_state).transpose()?;
    let jobs = state
        .state_store
        .with_state_db(|db| db.list_sync_jobs_by_state(job_state, req.limit))?;

    Ok(serde_json::json!({
        "jobs": jobs.into_iter().map(sync_job_to_json).collect::<Vec<_>>(),
    }))
}

pub(crate) fn parse_sync_job_state(value: &str) -> Result<ryeos_state::SyncJobState> {
    match value {
        "planned" => Ok(ryeos_state::SyncJobState::Planned),
        "running" => Ok(ryeos_state::SyncJobState::Running),
        "completed" => Ok(ryeos_state::SyncJobState::Completed),
        "failed" => Ok(ryeos_state::SyncJobState::Failed),
        "retryable" => Ok(ryeos_state::SyncJobState::Retryable),
        "cancelled" => Ok(ryeos_state::SyncJobState::Cancelled),
        other => anyhow::bail!("unknown sync job state: {other}"),
    }
}

pub(crate) fn sync_job_to_json(job: ryeos_state::SyncJobRecord) -> Value {
    serde_json::json!({
        "job_id": job.job_id,
        "operation_type": job.operation_type,
        "peer": job.peer,
        "state": job.state.as_str(),
        "phase": job.phase,
        "roots": job.roots,
        "heads": job.heads,
        "uploaded_hashes": job.uploaded_hashes,
        "fetched_hashes": job.fetched_hashes,
        "attempt_count": job.attempt_count,
        "max_attempts": job.max_attempts,
        "last_error": job.last_error,
        "result": job.result,
        "created_at": job.created_at,
        "updated_at": job.updated_at,
        "finished_at": job.finished_at,
    })
}

/// Exact inspection projection. The list surface remains a compact discovery
/// view; callers that select one durable coordinate may inspect the canonical
/// operation that owns its retry authority.
pub(crate) fn sync_job_inspect_to_json(job: ryeos_state::SyncJobRecord) -> Value {
    let operation = job.operation.clone();
    let mut value = sync_job_to_json(job);
    value["operation"] = operation;
    value
}

pub(crate) fn sync_job_attempt_to_json(attempt: ryeos_state::SyncJobAttemptRecord) -> Value {
    serde_json::json!({
        "attempt_id": attempt.attempt_id,
        "job_id": attempt.job_id,
        "attempt_number": attempt.attempt_number,
        "worker_id": attempt.worker_id,
        "state": attempt.state.as_str(),
        "phase": attempt.phase,
        "started_at": attempt.started_at,
        "updated_at": attempt.updated_at,
        "finished_at": attempt.finished_at,
        "error": attempt.error,
        "result": attempt.result,
    })
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:sync/jobs/list",
    endpoint: "sync.jobs.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.sync/jobs/list"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::{SyncJobRecord, SyncJobState};

    fn job() -> SyncJobRecord {
        SyncJobRecord {
            job_id: "remote-execute:test".to_string(),
            operation_type: "remote_execute".to_string(),
            operation: serde_json::json!({
                "schema": 1,
                "operation_type": "remote_execute",
                "item_ref": "tool:qualification/run",
            }),
            peer: Some("https://target.example".to_string()),
            state: SyncJobState::Completed,
            phase: "completed".to_string(),
            roots: Vec::new(),
            heads: Vec::new(),
            uploaded_hashes: vec!["sha256:source".to_string()],
            fetched_hashes: vec!["sha256:result".to_string()],
            attempt_count: 1,
            max_attempts: 1,
            last_error: None,
            result: None,
            created_at: "2026-09-01T00:00:00Z".to_string(),
            updated_at: "2026-09-01T00:00:01Z".to_string(),
            finished_at: Some("2026-09-01T00:00:01Z".to_string()),
        }
    }

    #[test]
    fn list_is_discovery_while_inspect_retains_exact_operation() {
        let listed = sync_job_to_json(job());
        assert!(listed.get("operation").is_none());

        let inspected = sync_job_inspect_to_json(job());
        assert_eq!(inspected["operation"]["item_ref"], "tool:qualification/run");
    }
}
