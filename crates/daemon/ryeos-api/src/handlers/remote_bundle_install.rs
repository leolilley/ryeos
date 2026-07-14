//! `remote/bundle-install` — install a bundle from a remote node.
//!
//! Orchestrator: calls `bundle_export` on the remote node, fetches all
//! file blobs via `objects_get`, materializes them into the local bundle
//! install directory, then runs preflight verification.
//!
//! Fail-closed: if any blob is missing or preflight fails, the entire
//! install is aborted and the partial directory is cleaned up.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::client::RemoteClient;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

const REMOTE_OBJECT_BATCH_DECLARED_BYTES: u64 = 8 * 1024 * 1024;
const REMOTE_OBJECT_BATCH_MAX_HASHES: usize = 256;
const REMOTE_OBJECT_RESPONSE_MAX_BYTES: usize = 48 * 1024 * 1024;

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
        let new_gen = state.engine_cache.bump_system_install_generation();
        tracing::info!(
            bundle = %req.bundle_name,
            engine_cache_generation = new_gen,
            "recovered remote bundle install: bumped engine cache generation"
        );
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

    // 2. Materialize a hidden generation in bounded CAS batches. No response
    // or decoded bundle-wide blob map is retained: at most one bounded batch
    // is live, and any missing/malformed blob removes the hidden generation.
    std::fs::create_dir_all(&bundles_root)
        .with_context(|| format!("create bundles root {}", bundles_root.display()))?;
    let staging = bundles_root.join(format!(".{}.remote-staging", req.bundle_name));
    let node_config_root = state.config.runtime_root().config();
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

    let staged_result: Result<((usize, u64), std::path::PathBuf)> = async {
        let counts = fetch_and_materialize_files(&client, &entries, &staging).await?;
        if let Err(error) = crate::handlers::bundle_install::admit_completed_staging(
            &state.config.app_root,
            &req.bundle_name,
            &staging,
            false,
            &node_config_root,
            &state.engine.node_trust_store,
        ) {
            bail!(
                "prospective admission failed for bundle '{}': {}",
                req.bundle_name,
                error
            );
        }
        lillux::sync_tree_durable(&staging)
            .with_context(|| format!("flush staged bundle {}", staging.display()))?;
        if local_target.exists() {
            bail!(
                "bundle '{}' appeared before remote activation at {}",
                req.bundle_name,
                local_target.display()
            );
        }
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
    }
    .await;
    let ((files_installed, total_bytes), canonical_target) = match staged_result {
        Ok(result) => result,
        Err(error) => {
            if staging.exists() {
                if let Err(cleanup_error) = std::fs::remove_dir_all(&staging) {
                    tracing::warn!(
                        path = %staging.display(),
                        error = %cleanup_error,
                        "failed to remove rejected remote bundle staging tree"
                    );
                }
            }
            return Err(error);
        }
    };

    // 3. Write signed node-config bundle registration.

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

/// Fetch and materialize all unique blobs in bounded batches. Reused hashes
/// are fetched once and may materialize at multiple declared paths.
async fn fetch_and_materialize_files(
    client: &RemoteClient,
    entries: &[BundleExportEntry],
    local_target: &std::path::Path,
) -> Result<(usize, u64)> {
    let (uses_by_hash, batches) = build_blob_fetch_batches(entries)?;
    let mut files_installed = 0usize;
    let mut total_bytes: u64 = 0;

    for hashes in batches {
        let response = client
            .objects_get_with_response_limit(&hashes, REMOTE_OBJECT_RESPONSE_MAX_BYTES)
            .await
            .context("fetch bounded remote bundle blob batch")?;
        let mut blob_data = HashMap::with_capacity(response.entries.len());
        for response_entry in response.entries {
            if response_entry.kind != "blob" {
                continue;
            }
            let encoded = response_entry
                .data
                .as_deref()
                .context("remote blob response omitted base64 data")?;
            let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
                .with_context(|| format!("decode blob {}", response_entry.hash))?;
            blob_data.insert(response_entry.hash, bytes);
        }

        for hash in hashes {
            let entry_indices = uses_by_hash
                .get(&hash)
                .with_context(|| format!("missing local export uses for blob {hash}"))?;
            let bytes = blob_data.get(&hash).ok_or_else(|| {
                anyhow::anyhow!("remote bundle CAS returned non-blob or missing entry for {hash}")
            })?;
            let actual_size = u64::try_from(bytes.len()).context("remote blob size exceeds u64")?;
            let actual_hash = lillux::cas::sha256_hex(bytes);
            if actual_hash != hash {
                bail!(
                    "remote bundle blob content hash mismatch: expected {}, found {}",
                    hash,
                    actual_hash
                );
            }

            for index in entry_indices {
                let entry = &entries[*index];
                if actual_size != entry.size {
                    bail!(
                        "remote bundle entry '{}' declared size {} but fetched {} bytes",
                        entry.path,
                        entry.size,
                        actual_size
                    );
                }
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
        }
    }

    Ok((files_installed, total_bytes))
}

fn build_blob_fetch_batches(
    entries: &[BundleExportEntry],
) -> Result<(HashMap<String, Vec<usize>>, Vec<Vec<String>>)> {
    let mut uses_by_hash: HashMap<String, Vec<usize>> = HashMap::new();
    let mut ordered_hashes: Vec<(String, u64)> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        match uses_by_hash.get_mut(&entry.hash) {
            Some(indices) => {
                let first = &entries[indices[0]];
                if first.size != entry.size {
                    bail!(
                        "remote bundle reuses hash {} with conflicting sizes {} and {}",
                        entry.hash,
                        first.size,
                        entry.size
                    );
                }
                indices.push(index);
            }
            None => {
                uses_by_hash.insert(entry.hash.clone(), vec![index]);
                ordered_hashes.push((entry.hash.clone(), entry.size));
            }
        }
    }

    let mut batches = Vec::new();
    let mut current = Vec::new();
    let mut current_bytes = 0u64;
    for (hash, size) in ordered_hashes {
        let next_bytes = current_bytes
            .checked_add(size)
            .context("remote bundle batch size overflow")?;
        if !current.is_empty()
            && (current.len() >= REMOTE_OBJECT_BATCH_MAX_HASHES
                || next_bytes > REMOTE_OBJECT_BATCH_DECLARED_BYTES)
        {
            batches.push(std::mem::take(&mut current));
            current_bytes = 0;
        }
        current_bytes = current_bytes
            .checked_add(size)
            .context("remote bundle batch size overflow")?;
        current.push(hash);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    Ok((uses_by_hash, batches))
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
    if response.entries.len() > super::bundle_export::MAX_EXPORTED_BUNDLE_FILES {
        bail!(
            "remote bundle export contains {} files; maximum is {}",
            response.entries.len(),
            super::bundle_export::MAX_EXPORTED_BUNDLE_FILES
        );
    }
    if response.total_bytes > super::bundle_export::MAX_EXPORTED_BUNDLE_TOTAL_BYTES {
        bail!(
            "remote bundle export declares {} bytes; maximum is {}",
            response.total_bytes,
            super::bundle_export::MAX_EXPORTED_BUNDLE_TOTAL_BYTES
        );
    }

    let mut paths = HashSet::with_capacity(response.entries.len());
    let mut directories = HashSet::new();
    let mut declared_total = 0u64;
    for entry in &response.entries {
        if entry.kind != super::bundle_export::EXPORTED_BUNDLE_ENTRY_KIND_FILE {
            bail!(
                "remote bundle export entry '{}' has unsupported kind '{}'; only regular files are accepted",
                entry.path,
                entry.kind
            );
        }
        if entry.path.len() > super::bundle_export::MAX_EXPORTED_BUNDLE_PATH_BYTES {
            bail!(
                "remote bundle export entry path exceeds {} bytes: {}",
                super::bundle_export::MAX_EXPORTED_BUNDLE_PATH_BYTES,
                entry.path
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
        let components = entry.path.split('/').collect::<Vec<_>>();
        if components.len() > super::bundle_export::MAX_EXPORTED_BUNDLE_DEPTH + 1 {
            bail!(
                "remote bundle export path '{}' exceeds maximum tree depth of {}",
                entry.path,
                super::bundle_export::MAX_EXPORTED_BUNDLE_DEPTH
            );
        }
        let mut directory = String::new();
        for component in &components[..components.len().saturating_sub(1)] {
            if !directory.is_empty() {
                directory.push('/');
            }
            directory.push_str(component);
            if paths.contains(directory.as_str()) {
                bail!(
                    "remote bundle export path '{}' traverses file entry '{}'",
                    entry.path,
                    directory
                );
            }
            if directories.insert(directory.clone())
                && directories.len() + paths.len() >= super::bundle_export::MAX_EXPORTED_BUNDLE_TREE_ENTRIES
            {
                bail!(
                    "remote bundle export exceeds maximum of {} materialized files/directories",
                    super::bundle_export::MAX_EXPORTED_BUNDLE_TREE_ENTRIES
                );
            }
        }
        if directories.contains(entry.path.as_str()) {
            bail!(
                "remote bundle export file entry '{}' conflicts with an existing directory path",
                entry.path
            );
        }
        if !paths.insert(entry.path.clone()) {
            bail!(
                "remote bundle export contains duplicate path '{}'",
                entry.path
            );
        }
        if directories.len() + paths.len() > super::bundle_export::MAX_EXPORTED_BUNDLE_TREE_ENTRIES {
            bail!(
                "remote bundle export exceeds maximum of {} materialized files/directories",
                super::bundle_export::MAX_EXPORTED_BUNDLE_TREE_ENTRIES
            );
        }
        if !lillux::cas::valid_hash(&entry.hash) {
            bail!(
                "remote bundle export entry '{}' has invalid blob hash '{}'",
                entry.path,
                entry.hash
            );
        }
        if entry.size > super::bundle_export::MAX_EXPORTED_BUNDLE_FILE_BYTES {
            bail!(
                "remote bundle export entry '{}' declares {} bytes; per-file maximum is {}",
                entry.path,
                entry.size,
                super::bundle_export::MAX_EXPORTED_BUNDLE_FILE_BYTES
            );
        }
        validate_unix_file_mode(entry.mode, &entry.path)?;
        declared_total = declared_total
            .checked_add(entry.size)
            .context("remote bundle export total size overflow")?;
        if declared_total > super::bundle_export::MAX_EXPORTED_BUNDLE_TOTAL_BYTES {
            bail!(
                "remote bundle export entries exceed maximum total size of {} bytes",
                super::bundle_export::MAX_EXPORTED_BUNDLE_TOTAL_BYTES
            );
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, hash_byte: u8, size: u64) -> BundleExportEntry {
        BundleExportEntry {
            kind: super::super::bundle_export::EXPORTED_BUNDLE_ENTRY_KIND_FILE.to_string(),
            path: path.to_string(),
            hash: format!("{hash_byte:02x}").repeat(32),
            size,
            mode: 0o644,
        }
    }

    fn response(entries: Vec<BundleExportEntry>) -> BundleExportResponse {
        BundleExportResponse {
            bundle_name: "demo".to_string(),
            _bundle_path: "/remote/demo".to_string(),
            file_count: entries.len(),
            total_bytes: entries.iter().map(|entry| entry.size).sum(),
            entries,
        }
    }

    #[test]
    fn export_validation_rejects_per_file_limit() {
        let response = response(vec![entry(
            "large.bin",
            1,
            super::super::bundle_export::MAX_EXPORTED_BUNDLE_FILE_BYTES + 1,
        )]);
        assert!(validate_export_response(&response, "demo").is_err());
    }

    #[test]
    fn export_validation_rejects_path_limit() {
        let path = "a".repeat(super::super::bundle_export::MAX_EXPORTED_BUNDLE_PATH_BYTES + 1);
        let response = response(vec![entry(&path, 1, 1)]);
        assert!(validate_export_response(&response, "demo").is_err());
    }

    #[test]
    fn export_validation_rejects_excessive_tree_depth() {
        let mut components = vec![
            "directory";
            super::super::bundle_export::MAX_EXPORTED_BUNDLE_DEPTH + 1
        ];
        components.push("file");
        let path = components.join("/");
        let response = response(vec![entry(&path, 1, 1)]);
        assert!(validate_export_response(&response, "demo").is_err());
    }

    #[test]
    fn export_validation_rejects_file_directory_collision() {
        let response = response(vec![entry("a", 1, 1), entry("a/b", 2, 1)]);
        assert!(validate_export_response(&response, "demo").is_err());
    }

    #[test]
    fn fetch_batches_deduplicate_blob_hashes() {
        let entries = vec![entry("a", 1, 4), entry("b", 1, 4), entry("c", 2, 4)];
        let (uses, batches) = build_blob_fetch_batches(&entries).unwrap();
        assert_eq!(uses.get(&entries[0].hash).unwrap(), &vec![0, 1]);
        assert_eq!(batches.iter().map(Vec::len).sum::<usize>(), 2);
    }

    #[test]
    fn fetch_batches_reject_conflicting_sizes_for_same_hash() {
        let entries = vec![entry("a", 1, 4), entry("b", 1, 5)];
        assert!(build_blob_fetch_batches(&entries).is_err());
    }
}
