//! Target-local admission-token minting through retained node authority.
//!
//! This is a normal signed Service, available against a live daemon and via
//! the existing stopped-node service runner. It must not be an isolated Tool:
//! minting writes node-private admission state and consumes the exact hosted
//! policy generation already loaded by the daemon.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::node_policy::sections::hosted::HostedNodePolicy;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

pub type Request = ryeos_core_tools::actions::authorize::MintAdmissionTokenRequest;

pub async fn handle(
    request: Request,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    ryeos_app::operator_authority::require_local_configured_operator(&state, &context)
        .context("admission-token minting requires the configured local operator")?;
    let params = request.into_params(state.config.app_root.clone())?;
    let policy = state
        .node_policy
        .require::<HostedNodePolicy>()
        .context("load retained hosted-node policy")?
        .clone();
    let policy_source = state
        .node_policy
        .source_file::<HostedNodePolicy>()
        .context("load retained hosted-node policy source")?
        .to_path_buf();
    tokio::task::spawn_blocking(move || {
        let result = ryeos_core_tools::actions::authorize::mint_admission_token_with_policy(
            params,
            &policy,
            &policy_source,
        )?;
        serde_json::to_value(result).map_err(Into::into)
    })
    .await
    .context("admission-token mint worker stopped")?
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:identity/mint-admission-token",
    endpoint: "identity.mint-admission-token",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.identity/mint-admission-token"],
    handler: |params, context, state| {
        Box::pin(async move {
            let request = crate::handler_error::parse_request(params)?;
            handle(request, context, state).await
        })
    },
};
