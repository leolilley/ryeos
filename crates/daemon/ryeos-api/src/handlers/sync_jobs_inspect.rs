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
    // Job state/count and the retained attempt suffix are one observation.
    // Keep the StateStore mutation guard across both reads so a concurrent
    // reservation, settlement, or compaction cannot manufacture an
    // internally impossible inspect response.
    let (job, attempts) = state.state_store.with_state_db(|db| {
        let job = db.get_sync_job(&req.job_id)?;
        let attempts = match job.as_ref() {
            Some(job) => db.list_sync_job_attempts(&job.job_id)?,
            None => Vec::new(),
        };
        Ok((job, attempts))
    })?;
    let Some(job) = job else {
        return Ok(serde_json::json!({
            "job_id": req.job_id,
            "status": "missing",
        }));
    };
    let retained_count = u64::try_from(attempts.len())?;
    let attempt_retention = if job.attempts_are_unbounded() {
        serde_json::json!({
            "mode": "bounded_terminal_suffix",
            "cumulative_count": job.attempt_count,
            "retained_count": retained_count,
            "terminal_row_limit": ryeos_state::SYNC_JOB_UNBOUNDED_RETAINED_TERMINAL_ATTEMPTS,
        })
    } else {
        serde_json::json!({
            "mode": "complete",
            "cumulative_count": job.attempt_count,
            "retained_count": retained_count,
            "terminal_row_limit": null,
        })
    };

    Ok(serde_json::json!({
        "status": "found",
        "job": crate::handlers::sync_jobs_list::sync_job_inspect_to_json(job),
        "attempt_retention": attempt_retention,
        "attempts": attempts
            .into_iter()
            .map(crate::handlers::sync_jobs_list::sync_job_attempt_to_json)
            .collect::<Vec<_>>(),
    }))
}

/// Sync jobs are the node's own, with no owner on the record: a job names
/// its peer, its roots and heads, and for a handoff the principal it runs for.
/// Only the node's admitted operator reads them; any other key is refused with
/// the typed operator refusal.
pub async fn handle_for_operator(
    req: Request,
    ctx: &ryeos_app::handler_context::HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    ryeos_app::operator_authority::require_admitted_operator(&state, ctx)?;
    handle(req, state).await
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:sync/jobs/inspect",
    endpoint: "sync.jobs.inspect",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.sync/jobs/inspect"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle_for_operator(req, &ctx, state).await
        })
    },
};
