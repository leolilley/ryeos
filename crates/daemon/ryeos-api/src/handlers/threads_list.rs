//! `threads.list` — paginated thread listing.
//!
//! Supports owner filtering: authenticated callers see only threads they own.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

use super::default_list_limit;
use crate::handler_context::HandlerContext;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default = "default_list_limit")]
    pub limit: usize,
    /// Row order: `default` (oldest-first), `newest` (newest-first — the
    /// "what just ran" order; the limit truncates, so oldest-first + limit
    /// returns the OLDEST rows), or `watch` (active-first, then newest).
    #[serde(default)]
    pub sort: Option<String>,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let Some(filter_principal) = ctx.is_present().then_some(ctx.fingerprint.as_str()) else {
        return Ok(serde_json::json!([]));
    };
    let sort = match req.sort.as_deref() {
        None | Some("default") => ryeos_app::thread_lifecycle::ThreadSort::Default,
        Some("newest") => ryeos_app::thread_lifecycle::ThreadSort::Newest,
        Some("watch") => ryeos_app::thread_lifecycle::ThreadSort::Watch,
        Some(other) => anyhow::bail!(
            "invalid sort '{other}': expected one of default | newest | watch"
        ),
    };
    state
        .threads
        .list_threads_filtered_sorted(req.limit, Some(filter_principal), sort)
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
