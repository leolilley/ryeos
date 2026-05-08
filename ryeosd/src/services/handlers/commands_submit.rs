//! `commands.submit` — enqueue a command for an existing thread.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::services::command_service::CommandSubmitParams;
use crate::state::AppState;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub thread_id: String,
    pub command_type: String,
    #[serde(default)]
    pub requested_by: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let record = state.commands.submit(&CommandSubmitParams {
        thread_id: req.thread_id,
        command_type: req.command_type,
        requested_by: req.requested_by,
        params: req.params,
    })?;
    serde_json::to_value(record).map_err(Into::into)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:commands/submit",
    endpoint: "commands.submit",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.commands/submit"],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)?;
            handle(req, state).await
        })
    },
};
