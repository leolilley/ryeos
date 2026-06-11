//! `remote/pull` — fetch arbitrary CAS objects from a remote node.
//!
//! Thin CLI wrapper over `RemoteClient::objects_get`. Fetches objects
//! by hash and stores them in the local CAS. Optionally materializes
//! them to an output directory.

use std::sync::Arc;

use anyhow::{bail, Result};
use base64::Engine as _;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::client::RemoteClient;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

fn default_remote() -> String {
    "default".to_string()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Remote config name.
    #[serde(default = "default_remote")]
    pub remote: String,
    /// SHA-256 hex hashes to fetch. Accepts a single string or array.
    #[serde(deserialize_with = "ryeos_runtime::scalar_or_vec::deserialize")]
    pub hashes: Vec<String>,
    /// Optional local directory to materialize objects into.
    /// When unset, objects are stored in the local CAS only.
    #[serde(default)]
    pub output_dir: Option<String>,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    if req.hashes.is_empty() {
        anyhow::bail!("hashes must not be empty");
    }

    let client = RemoteClient::from_named_remote(&state, &req.remote, None)?;
    let resp = client.objects_get(&req.hashes).await?;

    let mut fetched = 0usize;
    let mut stored_hashes: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();

    let cas_root = state.state_store.cas_root()?;
    let cas = lillux::cas::CasStore::new(cas_root);

    for entry in &resp.entries {
        match entry.kind.as_str() {
            "blob" => {
                let bytes = entry
                    .data
                    .as_deref()
                    .map(|b64| base64::engine::general_purpose::STANDARD.decode(b64))
                    .transpose()
                    .map_err(|e| anyhow::anyhow!("invalid base64 for blob {}: {e}", entry.hash))?
                    .ok_or_else(|| anyhow::anyhow!("blob {} missing data field", entry.hash))?;

                let stored = cas.store_blob(&bytes)?;
                stored_hashes.push(stored.clone());
                fetched += 1;

                if let Some(ref dir) = req.output_dir {
                    let out_path = std::path::Path::new(dir).join(&entry.hash);
                    std::fs::create_dir_all(dir)?;
                    std::fs::write(&out_path, &bytes)?;
                }
            }
            "object" => {
                let value = entry
                    .value
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("object {} missing value field", entry.hash))?;
                let stored = cas.store_object(value)?;
                stored_hashes.push(stored.clone());
                fetched += 1;

                if let Some(ref dir) = req.output_dir {
                    let out_path = std::path::Path::new(dir).join(format!("{}.json", entry.hash));
                    std::fs::create_dir_all(dir)?;
                    let content = serde_json::to_string_pretty(value)?;
                    std::fs::write(&out_path, content)?;
                }
            }
            "missing" => {
                missing.push(entry.hash.clone());
            }
            _ => {}
        }
    }

    // Fail-closed: if any requested hash was not found on the remote,
    // abort and report all missing hashes.
    if !missing.is_empty() {
        bail!(
            "remote.pull: {} of {} requested hashes not found on remote: {}",
            missing.len(),
            req.hashes.len(),
            missing.join(", ")
        );
    }

    Ok(serde_json::json!({
        "fetched": fetched,
        "requested": req.hashes.len(),
        "hashes": stored_hashes,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/pull",
    endpoint: "remote.pull",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.objects/get"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};
