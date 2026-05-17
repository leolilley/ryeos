//! `remote/bundle-install` — install a bundle from a remote node.
//!
//! Orchestrator: calls `bundle_export` on the remote node, fetches all
//! file blobs via `objects_get`, then materializes them into the local
//! bundle install directory.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::remote::client::RemoteClient;
use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;

fn default_remote() -> String { "default".to_string() }

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Remote config name.
    #[serde(default = "default_remote")]
    pub remote: String,
    /// Bundle name to export from the remote node.
    pub bundle_name: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    // Validate bundle name locally before hitting the network.
    crate::handlers::bundle_install::validate_name(&req.bundle_name)?;

    // Check bundle doesn't already exist locally.
    let bundles_root = state.config.system_space_dir
        .join(ryeos_engine::AI_DIR)
        .join("bundles");
    let local_target = bundles_root.join(&req.bundle_name);
    if local_target.exists() {
        bail!(
            "bundle '{}' already installed locally at {}",
            req.bundle_name,
            local_target.display()
        );
    }

    let client = RemoteClient::from_named_remote(&state, &req.remote)?;

    // 1. Call bundle_export on the remote.
    let export_resp = client.bundle_export(&req.bundle_name).await?;

    let entries = export_resp["entries"].as_array()
        .cloned()
        .unwrap_or_default();

    if entries.is_empty() {
        bail!("remote bundle '{}' has no files", req.bundle_name);
    }

    // 2. Collect all hashes and fetch from remote CAS.
    let hashes: Vec<String> = entries.iter()
        .filter_map(|e| e["hash"].as_str().map(String::from))
        .collect();

    // Use the typed objects_get response for reliable blob decoding.
    let get_resp = client.objects_get(&hashes).await?;

    // Index CAS entries by hash for quick lookup.
    let mut blob_data: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    for entry in &get_resp.entries {
        if entry.kind == "blob" {
            if let Some(ref b64) = entry.data {
                let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
                    .with_context(|| format!("decode blob {}", entry.hash))?;
                blob_data.insert(entry.hash.clone(), bytes);
            }
        }
    }

    // 3. Materialize to local bundle directory.
    std::fs::create_dir_all(&local_target)
        .with_context(|| format!("create bundle dir {}", local_target.display()))?;

    let mut files_installed = 0usize;
    let mut total_bytes: u64 = 0;

    for entry in &entries {
        let rel_path = entry["path"].as_str().unwrap_or("");
        let hash = entry["hash"].as_str().unwrap_or("");

        let bytes = match blob_data.get(hash) {
            Some(b) => b,
            None => {
                tracing::warn!(path = rel_path, hash = hash, "skipping: blob not fetched");
                continue;
            }
        };

        let file_path = local_target.join(rel_path);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
        std::fs::write(&file_path, bytes)
            .with_context(|| format!("write {}", file_path.display()))?;

        total_bytes += bytes.len() as u64;
        files_installed += 1;
    }

    // 4. Write signed node-config bundle registration.
    let canonical_target = local_target.canonicalize()
        .context("canonicalize installed bundle path")?;

    ryeos_app::node_config::writer::write_signed_node_item(
        &state.config.system_space_dir.join(ryeos_engine::AI_DIR).join("node"),
        "bundles",
        &req.bundle_name,
        &serde_json::json!({ "path": canonical_target }),
        &state.identity,
    )?;

    Ok(serde_json::json!({
        "bundle_name": req.bundle_name,
        "files_installed": files_installed,
        "total_bytes": total_bytes,
        "path": canonical_target.display().to_string(),
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/bundle-install",
    endpoint: "remote.bundle_install",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle/install"],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};
