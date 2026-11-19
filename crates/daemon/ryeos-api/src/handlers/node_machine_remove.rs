//! `node.machine.remove` — remove a machine declaration.
//!
//! Deletes the node-signed record under `.ai/node/machines/`, whether or not it
//! still verifies, so a broken record can be cleared. Only the configured local
//! operator may remove one, as for declaring.

use std::sync::Arc;

use anyhow::{Context, Result};
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::machine_declaration;
use crate::registry::ServiceDescriptor;

/// The capability a caller needs to remove a machine declaration.
pub const CAPABILITY: &str = "ryeos.execute.service.node/machine/remove";

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub label: String,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    ryeos_app::operator_authority::require_local_configured_operator(&state, &ctx)
        .context("machine remove requires the configured local operator")?;
    let path =
        machine_declaration::remove_declaration(state.config.runtime_root().as_path(), &req.label)?;
    Ok(serde_json::json!({ "removed": req.label, "config_path": path }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:node/machine/remove",
    endpoint: "node.machine.remove",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[CAPABILITY],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_names_one_capability() {
        assert_eq!(DESCRIPTOR.required_caps, &[CAPABILITY]);
        assert_eq!(DESCRIPTOR.service_ref, "service:node/machine/remove");
    }
}
