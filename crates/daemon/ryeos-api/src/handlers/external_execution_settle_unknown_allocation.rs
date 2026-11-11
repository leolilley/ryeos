//! Operator statement releasing one unknown external allocation outcome.

use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub placement_thread_id: String,
    pub reason: String,
    pub evidence: String,
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-execution/settle-unknown-allocation",
    endpoint: "external-execution.settle-unknown-allocation",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-execution/settle-unknown-allocation"],
    handler: |params, ctx, state: Arc<AppState>| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)?;
            let phase = state.state_store.settle_unknown_external_allocation(
                &req.placement_thread_id,
                &ctx.fingerprint,
                &req.reason,
                &req.evidence,
            )?;
            tracing::warn!(
                placement = %req.placement_thread_id,
                principal = %ctx.fingerprint,
                reason = %req.reason,
                "operator settled an unknown external allocation outcome"
            );
            let value: Result<Value> = Ok(serde_json::json!({
                "placement_thread_id": req.placement_thread_id,
                "phase": phase,
                "principal": ctx.fingerprint,
            }));
            value
        })
    },
};
