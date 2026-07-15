//! `project/status` — report the deployed snapshot for a live project.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::objects::{ProjectSnapshot, SourceManifest};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub project_path: String,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    ctx.require_verified().map_err(|e| anyhow!(e))?;

    let canonical = crate::handlers::project_apply_snapshot::canonical_existing_project_path(
        &req.project_path,
    )?;
    let canonical_project_path = canonical
        .to_str()
        .ok_or_else(|| anyhow!("canonical project_path is not valid UTF-8"))?
        .to_owned();
    let project_hash = ryeos_state::refs::deployed_project_key(&canonical_project_path);
    let cas_read = state.acquire_cas_read()?;

    let deployed = state
        .state_store
        .with_state_db(|db| db.read_deployed_project_ref(&project_hash))?;

    let Some(deployed) = deployed else {
        return Ok(serde_json::json!({
            "project_path": canonical_project_path,
            "project_hash": project_hash,
            "deployed": false,
            "deployed_snapshot_hash": null,
        }));
    };

    let cas = cas_read.cas();
    let snapshot_obj = cas.get_object(&deployed.target_hash)?.ok_or_else(|| {
        anyhow!(
            "deployed snapshot {} not found in CAS",
            deployed.target_hash
        )
    })?;
    let snapshot = ProjectSnapshot::from_value(&snapshot_obj)?;
    let manifest_obj = cas
        .get_object(&snapshot.project_manifest_hash)?
        .ok_or_else(|| {
            anyhow!(
                "manifest {} not found in CAS",
                snapshot.project_manifest_hash
            )
        })?;
    let manifest = SourceManifest::from_value(&manifest_obj)?;

    Ok(serde_json::json!({
        "project_path": canonical_project_path,
        "project_hash": project_hash,
        "deployed": true,
        "deployed_snapshot_hash": deployed.target_hash,
        "deployed_at": deployed.updated_at,
        "project_sync_scope": snapshot.project_sync_scope,
        "manifest_hash": snapshot.project_manifest_hash,
        "manifest_entries": manifest.item_source_hashes.len(),
        "snapshot_created_at": snapshot.created_at,
        "snapshot_source": snapshot.source,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:project/status",
    endpoint: "project.status",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.project/status"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};
