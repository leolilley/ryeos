//! `project/owner`: the project owner this node resolves for the
//! authenticated key.
//!
//! A remote binding records this at bind time (`remote_owner`) and asserts
//! it on every later head read or advance. The answer comes only from this
//! node's ownership resolver over its own signed signer registry; the key's
//! grants, scopes and sessions are unaffected by it.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

/// Schema of the response.
pub const PROJECT_OWNER_RESPONSE_SCHEMA: u32 = 1;

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Request {}

pub async fn handle(_req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    ctx.require_verified().map_err(|e| anyhow!(e))?;
    let resolved = ryeos_app::project_owner::owner_for(&state, &ctx.fingerprint)?;
    Ok(serde_json::json!({
        "schema": PROJECT_OWNER_RESPONSE_SCHEMA,
        "owner": resolved.principal,
        // Evidence only: how the owner was resolved. Nothing decides on it.
        "via": resolved.via,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:project/owner",
    endpoint: "project.owner",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.project/owner"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};
