//! `maintenance.gc` — async GC under the write barrier.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::state::AppState;

pub type Request = ryeos_state::gc::GcParams;

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let result = crate::maintenance::run_maintenance_gc(&state, &req).await?;
    serde_json::to_value(result).map_err(Into::into)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:maintenance/gc",
    endpoint: "maintenance.gc",
    availability: ServiceAvailability::Both,
    required_caps: &["rye.execute.service.maintenance/gc"],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)
                .map_err(|e| anyhow::anyhow!("invalid gc params: {e}"))?;
            handle(req, state).await
        })
    },
};
