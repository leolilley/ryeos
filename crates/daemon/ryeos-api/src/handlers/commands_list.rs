//! `commands.list` — the node's command records, for completion.
//!
//! The grammar shown is the grammar held: each record carries whether the
//! calling session may invoke it, evaluated daemon-side. Clients derive
//! completion purely from this data.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_runtime::authorizer::AuthorizationPolicy;
use ryeos_runtime::CommandDispatch;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {}

pub async fn handle(
    _req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    ctx.require_verified()?;
    let commands: Vec<Value> = state
        .command_registry
        .all_commands()
        .iter()
        .map(|c| {
            let invocable = match &c.dispatch {
                CommandDispatch::LocalHandler { .. } => false,
                CommandDispatch::Group => true,
                CommandDispatch::ExecuteRef { execute, .. } => {
                    let cap = format!("ryeos.execute.{}", execute.replacen(':', ".", 1));
                    let policy = AuthorizationPolicy::require_all(&[cap.as_str()]);
                    state.authorizer.authorize(&ctx.scopes, &policy).is_ok()
                }
                CommandDispatch::DirectExecuteItemRef { .. } => true,
            };
            json!({
                "name": c.name,
                "tokens": c.tokens,
                "description": c.description,
                "arguments": c.arguments.iter().map(|a| json!({
                    "name": a.name,
                    "kind": format!("{:?}", a.kind),
                    "required": a.required,
                    "description": a.description,
                })).collect::<Vec<_>>(),
                "invocable": invocable,
            })
        })
        .collect();
    Ok(json!({ "commands": commands }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:commands/list",
    endpoint: "commands.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};
