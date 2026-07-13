//! `remote/bundle-install` — install a bundle from a remote node.
//!
//! Orchestrator: calls `bundle_export` on the remote node, fetches all
//! file blobs via `objects_get`, materializes them into the local bundle
//! install directory, then runs preflight verification.
//!
//! Fail-closed: if any blob is missing or preflight fails, the entire
//! install is aborted and the partial directory is cleaned up.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
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
    /// Bundle name to export from the remote node.
    pub bundle_name: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    // Validate bundle name locally before hitting the network.
    crate::handlers::bundle_install::validate_name(&req.bundle_name)?;

    // Check bundle doesn't already exist locally.
    let bundles_root = state
        .config
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("bundles");
    let local_target = bundles_root.join(&req.bundle_name);
    let transaction = ryeos_app::bundle_transaction::BundleTransaction::acquire(
        &state.config.app_root,
        &req.bundle_name,
    )?;
    let recovered = transaction.reconcile(state.identity.signing_key())?;
    if matches!(
        recovered,
        Some(
            ryeos_app::bundle_transaction::BundleOperation::Install
                | ryeos_app::bundle_transaction::BundleOperation::RemoteInstall
        )
    ) && transaction.target().is_dir()
    {
        return Ok(serde_json::json!({
            "bundle_name": req.bundle_name,
            "path": transaction.target(),
            "recovered": true,
        }));
    }
    if local_target.exists() {
        bail!(
            "bundle '{}' already installed locally at {}",
            req.bundle_name,
            local_target.display()
        );
    }

    let client = RemoteClient::from_named_remote(&state, &req.remote, None)?;

    // 1. Call bundle_export on the remote.
    let export_resp = client.bundle_export(&req.bundle_name).await?;

    let entries = export_resp["entries"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    if entries.is_empty() {
        bail!("remote bundle '{}' has no files", req.bundle_name);
    }

    // 2. Collect all hashes and fetch from remote CAS.
    let hashes: Vec<String> = entries
        .iter()
        .filter_map(|e| e["hash"].as_str().map(String::from))
        .collect();

    // Use the typed objects_get response for reliable blob decoding.
    let get_resp = client.objects_get(&hashes).await?;

    // Index CAS entries by hash for quick lookup.
    let mut blob_data: std::collections::HashMap<String, Vec<u8>> =
        std::collections::HashMap::new();
    for entry in &get_resp.entries {
        if entry.kind == "blob" {
            if let Some(ref b64) = entry.data {
                let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
                    .with_context(|| format!("decode blob {}", entry.hash))?;
                blob_data.insert(entry.hash.clone(), bytes);
            }
        }
    }

    // 3. Fail-closed: verify ALL blobs are present before materializing.
    let mut missing: Vec<String> = Vec::new();
    for entry in &entries {
        let hash = entry["hash"].as_str().unwrap_or("");
        if !blob_data.contains_key(hash) {
            let path = entry["path"].as_str().unwrap_or("?");
            missing.push(format!("{} (hash={})", path, hash));
        }
    }
    if !missing.is_empty() {
        bail!(
            "remote bundle '{}' has {} missing blob(s); aborting install: {}",
            req.bundle_name,
            missing.len(),
            missing.join(", ")
        );
    }

    // 4. Materialize and verify a hidden generation, then expose the complete
    // tree with one durable rename.
    std::fs::create_dir_all(&bundles_root)
        .with_context(|| format!("create bundles root {}", bundles_root.display()))?;
    let staging = bundles_root.join(format!(".{}.remote-staging", req.bundle_name));
    let ((files_installed, total_bytes), canonical_target) = (|| {
        if local_target.exists() {
            bail!(
                "bundle '{}' appeared during remote install at {}",
                req.bundle_name,
                local_target.display()
            );
        }
        if staging.exists() {
            std::fs::remove_dir_all(&staging)
                .with_context(|| format!("remove stale staging {}", staging.display()))?;
        }
        std::fs::create_dir(&staging)
            .with_context(|| format!("create staging dir {}", staging.display()))?;
        let counts = materialize_files(&entries, &blob_data, &staging).inspect_err(|_| {
            let _ = std::fs::remove_dir_all(&staging);
        })?;
        if let Err(error) =
            ryeos_bundle::preflight::preflight_verify_bundle(&staging, &state.config.app_root)
        {
            let _ = std::fs::remove_dir_all(&staging);
            bail!(
                "preflight verification failed for bundle '{}': {}",
                req.bundle_name,
                error
            );
        }
        lillux::sync_tree_durable(&staging)
            .with_context(|| format!("flush staged bundle {}", staging.display()))?;
        let registration = serde_json::json!({ "kind": "node", "path": local_target });
        transaction.begin_present(
            ryeos_app::bundle_transaction::BundleOperation::RemoteInstall,
            &staging,
            registration,
        )?;
        lillux::rename_path_durable(&staging, &local_target)?;
        transaction.mark_activated()?;
        let canonical = local_target
            .canonicalize()
            .context("canonicalize installed bundle path")?;
        Ok((counts, canonical))
    })()?;

    // 5. Write signed node-config bundle registration.

    transaction
        .commit_present(state.identity.signing_key())
        .context("commit remote bundle registration")?;

    // Bump the engine cache generation — same as local bundle_install.
    let new_gen = state.engine_cache.bump_system_install_generation();
    tracing::info!(
        bundle = %req.bundle_name,
        engine_cache_generation = new_gen,
        "remote bundle installed: bumped engine cache generation"
    );

    Ok(serde_json::json!({
        "bundle_name": req.bundle_name,
        "files_installed": files_installed,
        "total_bytes": total_bytes,
        "path": canonical_target.display().to_string(),
    }))
}

/// Materialize all blob entries to the target directory.
/// Returns (files_installed, total_bytes) on success.
fn materialize_files(
    entries: &[Value],
    blob_data: &std::collections::HashMap<String, Vec<u8>>,
    local_target: &std::path::Path,
) -> Result<(usize, u64)> {
    let mut files_installed = 0usize;
    let mut total_bytes: u64 = 0;

    for entry in entries {
        let rel_path = entry["path"].as_str().unwrap_or("");
        let hash = entry["hash"].as_str().unwrap_or("");

        ryeos_state::project_sync::validate_safe_relative_path(rel_path)
            .with_context(|| format!("invalid exported bundle path '{rel_path}'"))?;

        let bytes = blob_data
            .get(hash)
            .ok_or_else(|| anyhow::anyhow!("blob {} missing after pre-check", hash))?;

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

    Ok((files_installed, total_bytes))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/bundle-install",
    endpoint: "remote.bundle-install",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle/install"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};
