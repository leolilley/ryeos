//! `threads.list` — paginated thread listing.
//!
//! Supports optional owner filtering: when the caller is authenticated
//! and is not an admin, only threads owned by the caller are returned.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;

use crate::handler_context::HandlerContext;
use super::default_list_limit;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default = "default_list_limit")]
    pub limit: usize,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    // Non-admin callers see only their own threads.
    let filter_principal = if ctx.is_present() && !ctx.is_admin() {
        Some(ctx.fingerprint.as_str())
    } else {
        None
    };
    state.threads.list_threads_filtered(req.limit, filter_principal)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:threads/list",
    endpoint: "threads.list",
    availability: ServiceAvailability::Both,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};
