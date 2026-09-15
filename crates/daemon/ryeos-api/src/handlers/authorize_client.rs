//! Local operator grant reconciliation through retained node authority.
//!
//! Grant publication belongs beside node identity, not inside an isolated
//! Tool that would need private node keys mounted into its workspace. Reuse
//! the canonical reconciliation transaction; never add an engine kind branch
//! or let a remote operator select this node's signing identity or app root.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

pub type Request = ryeos_core_tools::actions::authorize::AuthorizeClientRequest;

pub async fn handle(
    request: Request,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    ryeos_app::operator_authority::require_local_configured_operator(&state, &context)
        .context("grant reconciliation requires the configured local operator")?;
    let params = request.into_params(state.config.app_root.clone())?;
    tokio::task::spawn_blocking(move || {
        // Standalone dispatch already owns this exact lock. Borrow that
        // authority, not a boolean mode flag or a second flock acquisition.
        // The live daemon deliberately does not expose its lifecycle lock.
        let stopped_node = state.extensions.get::<ryeos_app::state_lock::StateLock>();
        let result = ryeos_core_tools::actions::authorize::run_authorize_client_with_authority(
            params,
            &state.identity,
            &state.config.authorized_keys_dir,
            stopped_node.as_deref(),
        )?;
        serde_json::to_value(result).map_err(Into::into)
    })
    .await
    .context("grant reconciliation worker stopped")?
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:identity/authorize-client",
    endpoint: "identity.authorize-client",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.identity/authorize-client"],
    handler: |params, context, state| {
        Box::pin(async move {
            let request = crate::handler_error::parse_request(params)?;
            handle(request, context, state).await
        })
    },
};
