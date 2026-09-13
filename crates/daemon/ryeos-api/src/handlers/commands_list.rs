//! `commands.list` — the node's command records, for completion.
//!
//! The grammar shown is the grammar held: each record carries whether the
//! calling session may invoke it, evaluated daemon-side. Clients derive
//! completion purely from this data.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_runtime::authorizer::AuthorizationPolicy;
use ryeos_runtime::{CommandDef, CommandDispatch};

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
            command_projection(c, invocable)
        })
        .collect();
    Ok(json!({ "commands": commands }))
}

fn command_projection(command: &CommandDef, invocable: bool) -> Value {
    json!({
        "name": command.name,
        "tokens": command.tokens,
        "description": command.description,
        "arguments": command.arguments.iter().map(|argument| json!({
            "name": argument.name,
            "kind": argument.kind,
            "positional": argument.positional,
            "required": argument.required,
            "arity": argument.arity,
            "description": argument.description,
        })).collect::<Vec<_>>(),
        "forms": command.forms,
        "defaults": command.defaults,
        "parameter_binding": command.parameter_binding,
        "control_flags": command.control_flags,
        "project": command.project,
        "invocable": invocable,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_exposes_complete_signed_command_grammar() {
        let command: CommandDef = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../bundles/core/.ai/node/commands/remote-worker-run.yaml"
        )))
        .unwrap();
        let projected = command_projection(&command, true);

        assert_eq!(projected["forms"].as_array().unwrap().len(), 2);
        assert_eq!(projected["defaults"]["remote"], "default");
        assert_eq!(projected["parameter_binding"]["input_flag"], "input");
        assert_eq!(projected["project"]["default"], "discover_upward_ai");
        assert_eq!(projected["project"]["request_project_path"], true);
        assert!(projected["control_flags"].as_array().unwrap().is_empty());
    }
}
