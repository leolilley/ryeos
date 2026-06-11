//! `sync/jobs/inspect` — inspect one durable distributed sync job.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub job_id: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let job = state
        .state_store
        .with_state_db(|db| db.get_sync_job(&req.job_id))?;
    let Some(job) = job else {
        return Ok(serde_json::json!({
            "job_id": req.job_id,
            "status": "missing",
        }));
    };
    let attempts = state
        .state_store
        .with_state_db(|db| db.list_sync_job_attempts(&job.job_id))?;

    Ok(serde_json::json!({
        "status": "found",
        "job": crate::handlers::sync_jobs_list::sync_job_to_json(job),
        "attempts": attempts
            .into_iter()
            .map(crate::handlers::sync_jobs_list::sync_job_attempt_to_json)
            .collect::<Vec<_>>(),
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:sync/jobs/inspect",
    endpoint: "sync.jobs.inspect",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.sync/jobs/inspect"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};
