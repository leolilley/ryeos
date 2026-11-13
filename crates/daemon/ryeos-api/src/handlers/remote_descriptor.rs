//! `remote/descriptor` — export this node's signed remote descriptor.
//!
//! Read-only. The daemon builds the descriptor from its own identity and vault
//! public key and signs it with the node key it holds; the key never reaches a
//! tool process.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::descriptor::{DescriptorRequest, build_signed_descriptor};
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Name callers should use for the remote.
    pub name: String,
    /// Public URL callers should use to reach this node.
    pub url: String,
    /// Informational capability labels advertised by this node.
    #[serde(
        default,
        deserialize_with = "ryeos_runtime::scalar_or_vec::deserialize"
    )]
    pub capabilities: Vec<String>,
    /// Optional assertion of the admission mode; hosted policy decides.
    #[serde(default)]
    pub admission_mode: Option<String>,
    /// Optional provider/operator label.
    #[serde(default)]
    pub provider_name: Option<String>,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let hosted_policy = ryeos_core_tools::actions::hosted_policy::load_hosted_policy(
        state.config.app_root.as_path(),
    )?;
    let vault_fingerprint = state
        .vault_fingerprint
        .as_deref()
        .context("this node has no vault public key; run `ryeos init` first")?;
    let (descriptor, yaml) = build_signed_descriptor(
        DescriptorRequest {
            name: req.name,
            url: req.url,
            capabilities: req.capabilities,
            admission_mode: req.admission_mode,
            provider_name: req.provider_name,
        },
        &state.identity,
        vault_fingerprint,
        &hosted_policy,
    )?;
    Ok(serde_json::json!({ "descriptor": descriptor, "yaml": yaml }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/descriptor",
    endpoint: "remote.descriptor",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/descriptor"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};
