//! `node.machine.list` — list the declared machines and the records that could
//! not be used.
//!
//! Reads the node-signed records under `.ai/node/machines/`. A record that
//! fails verification, and every record that shares a node with another, is
//! listed under `invalid_machines` with a stable code; it never hides the
//! others.

use std::sync::Arc;

use anyhow::Result;
use ryeos_app::state::AppState;
use ryeos_effect_contract::locality::{InvalidDeclaration, exclude_conflicting_declarations};
use ryeos_executor::executor::ServiceAvailability;
use serde_json::Value;

use crate::machine_declaration;
use crate::registry::ServiceDescriptor;

/// The capability a caller needs to list declarations.
pub const CAPABILITY: &str = "ryeos.execute.service.node/machine/list";

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Request {}

pub async fn handle(_req: Request, state: Arc<AppState>) -> Result<Value> {
    let report = machine_declaration::load_declarations(&state.config.app_root)?;
    let mut machines = Vec::new();
    let mut by_label = std::collections::BTreeMap::new();
    for loaded in &report.declarations {
        by_label.insert(
            loaded.declaration.label.clone(),
            (loaded.declared_at.clone(), loaded.path.clone()),
        );
    }
    let (usable, conflicting) = exclude_conflicting_declarations(
        report
            .declarations
            .into_iter()
            .map(|loaded| loaded.declaration)
            .collect(),
    );
    for declaration in usable {
        let (declared_at, path) = by_label.remove(&declaration.label).unwrap_or_default();
        machines.push(serde_json::json!({
            "label": declaration.label,
            "members": declaration.members,
            "declared_at": declared_at,
            "config_path": path,
        }));
    }
    let mut invalid: Vec<InvalidDeclaration> = report.invalid;
    invalid.extend(conflicting);
    invalid.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(serde_json::json!({
        "machines": machines,
        "invalid_machines": invalid,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:node/machine/list",
    endpoint: "node.machine.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[CAPABILITY],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = if params.is_null() {
                Request::default()
            } else {
                crate::handler_error::parse_request(params)?
            };
            handle(req, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_names_one_capability() {
        assert_eq!(DESCRIPTOR.required_caps, &[CAPABILITY]);
        assert_eq!(DESCRIPTOR.service_ref, "service:node/machine/list");
    }
}
