//! `maintenance.gc` — async GC under the write barrier.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

pub type Request = ryeos_state::gc::GcParams;

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let result = crate::maintenance::run_maintenance_gc(&state, &req).await?;
    serde_json::to_value(result).map_err(Into::into)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:maintenance/gc",
    endpoint: "maintenance.gc",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.maintenance/gc"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = if params.is_null() {
                Request::default()
            } else {
                serde_json::from_value(params)
                    .map_err(|e| anyhow::anyhow!("invalid gc params: {e}"))?
            };
            handle(req, state).await
        })
    },
};
