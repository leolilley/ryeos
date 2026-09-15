//! Configured-operator snapshot comparison through the existing node owner.
//!
//! This service is distinct from project.status (deployed snapshot metadata).
//! Runtime Tools retain their sealed project/callback route. Neither endpoint
//! supplies node private files or a synthetic callback to a subprocess.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub project_path: PathBuf,
    #[serde(default)]
    pub include_unchanged: bool,
    // The signed CLI command supplies its scan default. Direct service
    // callers select a budget explicitly; zero retains the existing
    // unlimited-scan request. There is no Rust-authored timeout fallback.
    pub time_budget_ms: u64,
}

pub async fn handle(
    request: Request,
    caller: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    tokio::task::spawn_blocking(move || {
        ryeos_app::runtime_project_snapshot_service::local_operator_status(
            &state,
            &caller,
            &request.project_path,
            request.include_unchanged,
            request.time_budget_ms,
        )
    })
    .await
    .context("snapshot status worker stopped")?
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:project/snapshot-status",
    endpoint: "project.snapshot-status",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.project/snapshot-status"],
    handler: |params, caller, state| {
        Box::pin(async move {
            let request = crate::handler_error::parse_request(params)?;
            handle(request, caller, state).await
        })
    },
};
