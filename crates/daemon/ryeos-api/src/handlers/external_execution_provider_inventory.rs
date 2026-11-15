//! `external-execution.provider-inventory`: list what the provider holds for
//! one installed binding's owner scope, join it with the node's journals and
//! return the digest of the node-signed receipt. Read only at the provider.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// `<section>/<id>`: an `external_execution` or
    /// `runtime_snapshot_production` binding.
    pub binding: String,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let (section, id) = req
        .binding
        .split_once('/')
        .filter(|(section, id)| !section.is_empty() && !id.is_empty() && !id.contains('/'))
        .context("binding must be `<section>/<id>`")?;
    let (section, id) = (section.to_owned(), id.to_owned());
    let report = tokio::task::spawn_blocking(move || {
        ryeos_app::provider_inventory::run_provider_inventory(&state, &ctx, &section, &id)
    })
    .await
    .map_err(|error| anyhow::anyhow!("provider inventory join failed: {error}"))??;
    Ok(serde_json::to_value(report)?)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-execution/provider-inventory",
    endpoint: "external-execution.provider-inventory",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-execution/provider-inventory"],
    handler: |params, ctx, state| {
        Box::pin(
            async move { handle(crate::handler_error::parse_request(params)?, ctx, state).await },
        )
    },
};
