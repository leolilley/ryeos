//! `bundle.remove` — remove a bundle via node-config deletion.
//!
//! Deletes the signed `kind: node` item at `<app_root>/.ai/node/bundles/<name>.yaml`
//! and the bundle directory at `<app_root>/.ai/bundles/<name>/`.
//!
//! OfflineOnly: the daemon must be stopped (engine reload not implemented).

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Bundle name (directory under `<app_root>/.ai/bundles/`).
    pub name: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    if req.name.is_empty() || req.name.len() > 64 {
        bail!(
            "invalid bundle name '{}': must be 1–64 characters",
            req.name
        );
    }
    if !req
        .name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        bail!(
            "invalid bundle name '{}': must contain only lowercase letters, digits, underscore, or hyphen",
            req.name
        );
    }

    let transaction = ryeos_app::bundle_transaction::BundleTransaction::acquire(
        &state.config.app_root,
        &req.name,
    )?;
    transaction.reconcile(state.identity.signing_key())?;

    // Delete the signed node-config item
    let config_item_path = state
        .config
        .app_root
        .join(".ai")
        .join("node")
        .join("bundles")
        .join(format!("{}.yaml", req.name));

    // Delete the bundle directory
    let bundle_dir = state
        .config
        .app_root
        .join(".ai")
        .join("bundles")
        .join(&req.name);

    let removed_config_item = config_item_path.exists();
    let removed_dir = bundle_dir.exists();
    transaction.begin_remove()?;
    transaction.commit_absent()?;

    // Bump the engine cache generation so any cached per-request
    // engines (built against the previous bundle set) are invalidated.
    let new_gen = state.engine_cache.bump_system_install_generation();
    tracing::info!(
        bundle = %req.name,
        engine_cache_generation = new_gen,
        "bundle removed: bumped engine cache generation"
    );

    Ok(serde_json::json!({
        "name": req.name,
        "removed_config_item": removed_config_item,
        "removed_dir": removed_dir,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle/remove",
    endpoint: "bundle.remove",
    availability: ServiceAvailability::OfflineOnly,
    required_caps: &["ryeos.execute.service.bundle/remove"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request =
                serde_json::from_value(params).context("bundle.remove requires { name }")?;
            handle(req, state).await
        })
    },
};
