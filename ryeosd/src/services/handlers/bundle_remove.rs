//! `bundle.remove` — remove a bundle via node-config deletion.
//!
//! Deletes the signed `kind: node` item at `<system_space_dir>/.ai/node/bundles/<name>.yaml`
//! and the bundle directory at `<system_space_dir>/.ai/bundles/<name>/`.
//!
//! OfflineOnly: the daemon must be stopped (engine reload not implemented).

use std::fs;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::state::AppState;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Bundle name (directory under `<system_space_dir>/.ai/bundles/`).
    pub name: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    if req.name.is_empty()
        || req
            .name
            .contains(|c: char| c == '/' || c == '\\' || c == '.' || c.is_whitespace())
    {
        bail!(
            "invalid bundle name '{}': must be non-empty and contain no path separators, \
             dots, or whitespace",
            req.name
        );
    }

    // Delete the signed node-config item
    let config_item_path = state
        .config
        .system_space_dir
        .join(".ai")
        .join("node")
        .join("bundles")
        .join(format!("{}.yaml", req.name));

    if !config_item_path.exists() {
        bail!(
            "bundle '{}' is not installed (config item not found at {})",
            req.name,
            config_item_path.display()
        );
    }

    fs::remove_file(&config_item_path).with_context(|| {
        format!(
            "failed to remove config item {}",
            config_item_path.display()
        )
    })?;

    // Delete the bundle directory
    let bundle_dir = state
        .config
        .system_space_dir
        .join(".ai")
        .join("bundles")
        .join(&req.name);

    let removed_dir = if bundle_dir.exists() {
        fs::remove_dir_all(&bundle_dir).with_context(|| {
            format!(
                "failed to remove bundle directory {}",
                bundle_dir.display()
            )
        })?;
        true
    } else {
        false
    };

    Ok(serde_json::json!({
        "name": req.name,
        "removed_config_item": true,
        "removed_dir": removed_dir,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle/remove",
    endpoint: "bundle.remove",
    availability: ServiceAvailability::OfflineOnly,
    required_caps: &["rye.execute.service.bundle/remove"],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)
                .context("bundle.remove requires { name }")?;
            handle(req, state).await
        })
    },
};
