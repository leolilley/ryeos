//! `rebuild` — rebuild the projection DB from CAS state.
//!
//! OfflineOnly: caller (run-service standalone path) holds the state lock;
//! this handler just ports the rebuild logic from `actions::rebuild`.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::state::AppState;

#[derive(Debug, serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Request {
    /// When true, follow up the rebuild with a reachability sweep so
    /// callers can sanity-check object/blob counts.
    pub verify: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct RebuildReport {
    pub chains_rebuilt: usize,
    pub threads_restored: usize,
    pub events_projected: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reachable_objects: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reachable_blobs: Option<usize>,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let inner = state.config.state_dir.join(".ai").join("state");
    let cas_root = inner.join("objects");
    let refs_root = inner.join("refs");
    let projection_path = inner.join("projection.sqlite3");

    let report = tokio::task::spawn_blocking(move || -> Result<RebuildReport> {
        let projection = ryeos_state::ProjectionDb::open(&projection_path)?;
        let report =
            ryeos_state::rebuild::rebuild_projection(&projection, &cas_root, &refs_root)?;
        let mut out = RebuildReport {
            chains_rebuilt: report.chains_rebuilt,
            threads_restored: report.threads_restored,
            events_projected: report.events_projected,
            reachable_objects: None,
            reachable_blobs: None,
        };
        if req.verify && report.chains_rebuilt > 0 {
            let reachable =
                ryeos_state::reachability::collect_reachable(&cas_root, &refs_root)?;
            out.reachable_objects = Some(reachable.object_hashes.len());
            out.reachable_blobs = Some(reachable.blob_hashes.len());
        }
        Ok(out)
    })
    .await??;

    serde_json::to_value(report).map_err(Into::into)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:rebuild",
    endpoint: "rebuild",
    availability: ServiceAvailability::OfflineOnly,
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = if params.is_null() {
                Request::default()
            } else {
                serde_json::from_value(params)?
            };
            handle(req, state).await
        })
    },
};
