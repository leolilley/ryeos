//! `remote/threads` — list threads on a remote node.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::handlers::remote_push::named_remote_client;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

use super::default_list_limit;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default = "default_remote")]
    pub remote: String,
    #[serde(default = "default_list_limit")]
    pub limit: usize,
}

fn default_remote() -> String {
    "default".to_string()
}

pub async fn handle(
    req: Request,
    ctx: crate::handler_context::HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let client = named_remote_client(&state, &req.remote, &ctx)?;
    let result = client.threads_list(req.limit).await.map_err(|error| {
        anyhow::Error::new(crate::remote::client::map_remote_call_error(
            error,
            "remote threads",
        ))
    });
    drop(client);
    result
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/threads",
    endpoint: "remote.threads",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/admin"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};
