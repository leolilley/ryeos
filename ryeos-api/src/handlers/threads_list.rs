//! `threads.list` — paginated thread listing.
//!
//! Supports optional owner filtering: when `_caller_fingerprint` is
//! present and the caller is not an admin, only threads owned by the
//! caller are returned.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;

use super::default_list_limit;
use super::ownership::is_admin;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default = "default_list_limit")]
    pub limit: usize,
    /// Injected by service_invocation for caller-aware filtering.
    #[serde(default)]
    pub _caller_fingerprint: String,
    /// Injected by service_invocation for caller-aware filtering.
    #[serde(default)]
    pub _caller_scopes: Vec<String>,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    // Non-admin callers see only their own threads.
    let filter_principal = if !req._caller_fingerprint.is_empty() && !is_admin(&req._caller_scopes) {
        Some(req._caller_fingerprint.as_str())
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
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};
