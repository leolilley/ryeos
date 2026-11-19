//! `node.machine.declare` — declare that some nodes are one machine.
//!
//! The declaration is the operator's word and is graded `declared`: it writes
//! one node-signed record under `.ai/node/machines/`, and no node admits
//! anything on it. Only the configured local operator may declare, as for
//! remote pins. Members are site ids; see [`crate::machine_declaration`].

use std::sync::Arc;

use anyhow::{Context, Result};
use ryeos_app::state::AppState;
use ryeos_effect_contract::locality::MachineDeclaration;
use ryeos_executor::executor::ServiceAvailability;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::machine_declaration;
use crate::registry::ServiceDescriptor;

/// The capability a caller needs to declare a machine.
pub const CAPABILITY: &str = "ryeos.execute.service.node/machine/declare";

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// The machine's label: a lowercase key, also the record's file stem.
    pub label: String,
    /// Site ids of the nodes on the machine. A single value or a list.
    #[serde(deserialize_with = "ryeos_runtime::scalar_or_vec::deserialize")]
    pub members: Vec<String>,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    // A declaration changes what the operator's view calls a machine, so only
    // the configured local operator may make one.
    ryeos_app::operator_authority::require_local_configured_operator(&state, &ctx)
        .context("machine declare requires the configured local operator")?;
    let declaration = MachineDeclaration {
        label: req.label,
        members: req.members,
    };
    let digest = lillux::cas::canonical_json(&serde_json::json!({
        "label": declaration.label,
        "members": declaration.members,
    }))
    .ok()
    .map(|canonical| lillux::cas::sha256_hex(canonical.as_bytes()));
    let path = machine_declaration::write_declaration(
        state.config.runtime_root().as_path(),
        state.identity.signing_key(),
        &declaration,
        digest,
    )?;
    Ok(serde_json::json!({
        "declared": declaration.label,
        "members": declaration.members,
        "config_path": path,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:node/machine/declare",
    endpoint: "node.machine.declare",
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
    fn members_accept_one_value_or_a_list() {
        let one: Request =
            serde_json::from_value(serde_json::json!({"label": "w", "members": "site:a"})).unwrap();
        assert_eq!(one.members, ["site:a"]);
        let many: Request = serde_json::from_value(
            serde_json::json!({"label": "w", "members": ["site:a", "site:b"]}),
        )
        .unwrap();
        assert_eq!(many.members, ["site:a", "site:b"]);
    }

    #[test]
    fn the_descriptor_names_one_capability() {
        assert_eq!(DESCRIPTOR.required_caps, &[CAPABILITY]);
        assert_eq!(DESCRIPTOR.service_ref, "service:node/machine/declare");
    }
}
