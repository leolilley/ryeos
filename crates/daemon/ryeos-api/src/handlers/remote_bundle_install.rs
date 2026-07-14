//! `remote/bundle-install` — install a bundle from a remote node.
//!
//! Orchestrator: calls `bundle_export` on the remote node, fetches all
//! file blobs via `objects_get`, materializes them into the local bundle
//! install directory, then runs preflight verification.
//!
//! Fail-closed: if any blob is missing or preflight fails, the entire
//! install is aborted and the partial directory is cleaned up.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
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

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleExportResponse {
    bundle_name: String,
    #[serde(rename = "bundle_path")]
    _bundle_path: String,
    file_count: usize,
    total_bytes: u64,
    entries: Vec<BundleExportEntry>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleExportEntry {
    kind: String,
    path: String,
    hash: String,
    size: u64,
    mode: u32,
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
    let export_resp: BundleExportResponse =
        serde_json::from_value(client.bundle_export(&req.bundle_name).await?)
            .context("decode remote bundle export response")?;
    validate_export_response(&export_resp, &req.bundle_name)?;
    let entries = export_resp.entries;

    if entries.is_empty() {
        bail!("remote bundle '{}' has no files", req.bundle_name);
    }

    // 2. Collect all hashes and fetch from remote CAS.
    let hashes: Vec<String> = entries.iter().map(|entry| entry.hash.clone()).collect();

    // Use the typed objects_get response for reliable blob decoding.
    let get_resp = client.objects_get(&hashes).await?;

    // Index CAS entries by hash for quick lookup.
    let mut blob_data: HashMap<String, Vec<u8>> = HashMap::new();
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
        if !blob_data.contains_key(&entry.hash) {
            missing.push(format!("{} (hash={})", entry.path, entry.hash));
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
    for entry in &entries {
        let bytes = blob_data
            .get(&entry.hash)
            .expect("missing blobs were rejected above");
        let actual_size = u64::try_from(bytes.len()).context("remote blob size exceeds u64")?;
        if actual_size != entry.size {
            bail!(
                "remote bundle entry '{}' declared size {} but fetched {} bytes",
                entry.path,
                entry.size,
                actual_size
            );
        }
        let actual_hash = lillux::cas::sha256_hex(bytes);
        if actual_hash != entry.hash {
            bail!(
                "remote bundle entry '{}' content hash mismatch: expected {}, found {}",
                entry.path,
                entry.hash,
                actual_hash
            );
        }
    }

    // 4. Materialize and verify a hidden generation, then expose the complete
    // tree with one durable rename.
    std::fs::create_dir_all(&bundles_root)
        .with_context(|| format!("create bundles root {}", bundles_root.display()))?;
    let staging = bundles_root.join(format!(".{}.remote-staging", req.bundle_name));
    let node_config_root = state.config.runtime_root().config();
    let installed_dependency_roots: Vec<PathBuf> =
        ryeos_bundle::installed::load_installed_bundle_records(&state.config.app_root)
            .context("preflight: load installed bundle registrations")?
            .into_iter()
            .filter(|record| record.name != req.bundle_name)
            .map(|record| record.bundle_root)
            .collect();
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
        let counts = materialize_files(&entries, &blob_data, &staging).map_err(|error| {
            let _ = std::fs::remove_dir_all(&staging);
            error
        })?;
        if let Err(error) = ryeos_bundle::preflight::preflight_verify_bundle_staging_in_context(
            &staging,
            &req.bundle_name,
            &installed_dependency_roots,
            &node_config_root,
        ) {
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
    entries: &[BundleExportEntry],
    blob_data: &HashMap<String, Vec<u8>>,
    local_target: &std::path::Path,
) -> Result<(usize, u64)> {
    let mut files_installed = 0usize;
    let mut total_bytes: u64 = 0;

    for entry in entries {
        let bytes = blob_data
            .get(&entry.hash)
            .ok_or_else(|| anyhow::anyhow!("blob {} missing after pre-check", entry.hash))?;

        let file_path = local_target.join(&entry.path);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
        materialize_regular_file(&file_path, bytes, entry.mode)?;

        total_bytes = total_bytes
            .checked_add(entry.size)
            .context("materialized bundle size overflow")?;
        files_installed += 1;
    }

    Ok((files_installed, total_bytes))
}

fn validate_export_response(
    response: &BundleExportResponse,
    expected_bundle_name: &str,
) -> Result<()> {
    if response.bundle_name != expected_bundle_name {
        bail!(
            "remote exported bundle '{}' when '{}' was requested",
            response.bundle_name,
            expected_bundle_name
        );
    }
    if response.file_count != response.entries.len() {
        bail!(
            "remote bundle export file count mismatch: declared {}, received {} entries",
            response.file_count,
            response.entries.len()
        );
    }

    let mut paths = HashSet::with_capacity(response.entries.len());
    let mut declared_total = 0u64;
    for entry in &response.entries {
        if entry.kind != super::bundle_export::EXPORTED_BUNDLE_ENTRY_KIND_FILE {
            bail!(
                "remote bundle export entry '{}' has unsupported kind '{}'; only regular files are accepted",
                entry.path,
                entry.kind
            );
        }
        ryeos_state::project_sync::validate_safe_relative_path(&entry.path)
            .with_context(|| format!("invalid exported bundle path '{}'", entry.path))?;
        if entry.path.split('/').any(str::is_empty) {
            bail!(
                "remote bundle export path '{}' is not in canonical relative form",
                entry.path
            );
        }
        if !paths.insert(entry.path.clone()) {
            bail!(
                "remote bundle export contains duplicate path '{}'",
                entry.path
            );
        }
        if !lillux::cas::valid_hash(&entry.hash) {
            bail!(
                "remote bundle export entry '{}' has invalid blob hash '{}'",
                entry.path,
                entry.hash
            );
        }
        validate_unix_file_mode(entry.mode, &entry.path)?;
        declared_total = declared_total
            .checked_add(entry.size)
            .context("remote bundle export total size overflow")?;
    }
    if declared_total != response.total_bytes {
        bail!(
            "remote bundle export byte count mismatch: declared {}, entries total {}",
            response.total_bytes,
            declared_total
        );
    }
    Ok(())
}

fn validate_unix_file_mode(mode: u32, path: &str) -> Result<()> {
    if mode & !0o777 != 0 {
        bail!(
            "remote bundle export entry '{}' has invalid Unix file mode {mode:#o}: special permission bits are forbidden",
            path
        );
    }
    Ok(())
}

fn materialize_regular_file(path: &std::path::Path, bytes: &[u8], mode: u32) -> Result<()> {
    validate_unix_file_mode(mode, &path.display().to_string())?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        lillux::atomic_write_with_mode(path, bytes, mode)
            .with_context(|| format!("write regular file {}", path.display()))?;
        // open(2) applies the process umask to the creation mode. Restore the
        // exact validated transport mode before final bundle preflight.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("restore Unix mode {mode:#o} on {}", path.display()))?;
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("read materialized file metadata {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            bail!(
                "materialized bundle entry {} is not a regular file",
                path.display()
            );
        }
        let actual_mode = metadata.permissions().mode() & 0o7777;
        if actual_mode != mode {
            bail!(
                "failed to restore Unix mode on {}: expected {mode:#o}, found {actual_mode:#o}",
                path.display()
            );
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, bytes, mode);
        bail!("remote bundle install requires Unix file-mode support")
    }
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
