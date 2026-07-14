//! `remote/bundle-install` — install a bundle from a remote node.
//!
//! Orchestrator: calls `bundle_export` on the remote node, fetches all
//! file blobs via `objects_get`, materializes them into the local bundle
//! install directory, then runs preflight verification.
//!
//! Fail-closed: if any blob is missing or preflight fails, the entire
//! install is aborted and the partial directory is cleaned up.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::client::RemoteClient;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

const REMOTE_OBJECT_BATCH_DECLARED_BYTES: u64 = 8 * 1024 * 1024;
const REMOTE_OBJECT_BATCH_MAX_HASHES: usize = 256;
const REMOTE_OBJECT_RESPONSE_MAX_BYTES: usize = 48 * 1024 * 1024;
const REMOTE_FETCH_STALE_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const REMOTE_FETCH_SCAVENGE_SCAN_LIMIT: usize = 256;
const REMOTE_FETCH_SCAVENGE_REMOVE_LIMIT: usize = 8;
const REMOTE_FETCH_HEARTBEAT_FILE: &str = ".ryeos-remote-fetch-heartbeat";

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

    let bundles_root = state
        .config
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("bundles");
    let local_target = bundles_root.join(&req.bundle_name);
    let node_config_root = state.config.runtime_root().config();
    let prospective_validator = state
        .extensions
        .get::<ryeos_app::prospective_admission::ProspectiveNodeConfigValidator>()
        .context("prospective node-config validator is not installed at the composition root")?;

    // Reconcile/check under the same global -> per-name lock order used by the
    // final mutation, then release both before opening a socket. This avoids a
    // needless transfer for a known installed generation without allowing any
    // remote wait to block unrelated bundle mutations.
    {
        let registry_lock = ryeos_app::bundle_transaction::BundleRegistryMutationLock::acquire(
            &state.config.app_root,
        )?;
        scavenge_stale_remote_fetches(&bundles_root)?;
        let transaction = registry_lock.acquire_bundle(&req.bundle_name)?;
        if let Some(report) = reconcile_remote_install(&req.bundle_name, &state, &transaction)? {
            return Ok(report);
        }
        if local_target.exists() {
            bail!(
                "bundle '{}' already installed locally at {}",
                req.bundle_name,
                local_target.display()
            );
        }
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

    // 2. Materialize a request-unique hidden generation in bounded CAS batches
    // while no bundle transaction lock is held. No response or decoded
    // bundle-wide blob map is retained: at most one bounded batch is live.
    std::fs::create_dir_all(&bundles_root)
        .with_context(|| format!("create bundles root {}", bundles_root.display()))?;
    let transfer_staging = bundles_root.join(format!(
        ".{}.remote-fetch-{}",
        req.bundle_name,
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&transfer_staging).with_context(|| {
        format!(
            "create unique remote transfer staging {}",
            transfer_staging.display()
        )
    })?;
    let mut staging_cleanup = RemoteStagingCleanup::new(transfer_staging.clone());
    write_remote_fetch_heartbeat(&transfer_staging)
        .context("initialize remote bundle transfer heartbeat")?;
    let (files_installed, total_bytes) =
        fetch_and_materialize_files(&client, &entries, &transfer_staging).await?;
    lillux::sync_tree_durable(&transfer_staging).with_context(|| {
        format!(
            "flush remote transfer staging {}",
            transfer_staging.display()
        )
    })?;
    write_remote_fetch_heartbeat(&transfer_staging)
        .context("refresh remote bundle transfer heartbeat after durable flush")?;

    // 3. Acquire the node-wide registry lock only after all remote I/O has
    // completed, then acquire the per-name lock. Reconcile, exact re-planning,
    // admission, activation, and registration remain serialized as one final
    // local mutation. The private transfer generation is intentionally outside
    // both locks.
    let registry_lock =
        ryeos_app::bundle_transaction::BundleRegistryMutationLock::acquire(&state.config.app_root)?;
    let transaction = registry_lock.acquire_bundle(&req.bundle_name)?;
    if let Some(report) = reconcile_remote_install(&req.bundle_name, &state, &transaction)? {
        return Ok(report);
    }
    if local_target.exists() {
        bail!(
            "bundle '{}' appeared during remote transfer at {}",
            req.bundle_name,
            local_target.display()
        );
    }

    // The transfer is now protected by the node-wide mutation lock, so its
    // out-of-band liveness marker can be removed before the generation is
    // admitted as bundle content.
    remove_remote_fetch_heartbeat(&transfer_staging)?;

    let transaction_staging = bundles_root.join(format!(".{}.remote-staging", req.bundle_name));
    remove_stale_transaction_staging(&transaction_staging)?;
    match lillux::rename_path_durable(&transfer_staging, &transaction_staging) {
        Ok(()) => staging_cleanup.retarget(transaction_staging.clone()),
        Err(error) => {
            if error.namespace_committed() {
                staging_cleanup.retarget(transaction_staging.clone());
                staging_cleanup.cleanup_now();
            }
            return Err(error).with_context(|| {
                format!(
                    "move remote transfer generation {} to transaction staging {}",
                    transfer_staging.display(),
                    transaction_staging.display()
                )
            });
        }
    }

    let admission = crate::handlers::bundle_install::admit_completed_staging(
        &state.config.app_root,
        &req.bundle_name,
        &transaction_staging,
        false,
        &node_config_root,
        &state.engine.node_trust_store,
        &prospective_validator,
        Arc::clone(&state.sandbox),
    );
    if let Err(error) = admission {
        // The canonical staging name is shared by transaction recovery. Remove
        // this request's rejected generation before releasing its lock.
        staging_cleanup.cleanup_now();
        return Err(error).with_context(|| {
            format!(
                "prospective admission failed for bundle '{}'",
                req.bundle_name
            )
        });
    }

    let mut cache_invalidated = false;
    let activation: Result<PathBuf> = (|| {
        let registration = serde_json::json!({ "kind": "node", "path": local_target });
        transaction.begin_present(
            ryeos_app::bundle_transaction::BundleOperation::RemoteInstall,
            &transaction_staging,
            registration,
        )?;
        match lillux::rename_path_durable(&transaction_staging, &local_target) {
            Ok(()) => staging_cleanup.disarm(),
            Err(error) => {
                if error.namespace_committed() {
                    staging_cleanup.disarm();
                }
                return Err(error).with_context(|| {
                    format!(
                        "activate remote bundle staging {} at {}",
                        transaction_staging.display(),
                        local_target.display()
                    )
                });
            }
        }

        // The target namespace is now visible. Invalidate immediately, before
        // any fallible journal/registration step can return an error.
        let new_gen = state.engine_cache.bump_system_install_generation();
        cache_invalidated = true;
        tracing::info!(
            bundle = %req.bundle_name,
            engine_cache_generation = new_gen,
            "remote bundle namespace activated: bumped engine cache generation"
        );

        transaction.mark_activated()?;
        let canonical = local_target
            .canonicalize()
            .context("canonicalize installed bundle path")?;
        transaction
            .commit_present(state.identity.signing_key())
            .context("commit remote bundle registration")?;
        Ok(canonical)
    })();

    let canonical_target = match activation {
        Ok(path) => path,
        Err(error) => {
            // A durable-rename error can report failure after committing the
            // namespace, and mark/registration writes can fail after a normal
            // rename. Never return with a visible generation and stale engines.
            if !cache_invalidated && local_target.is_dir() {
                let new_gen = state.engine_cache.bump_system_install_generation();
                tracing::warn!(
                    bundle = %req.bundle_name,
                    engine_cache_generation = new_gen,
                    error = %error,
                    "remote bundle activation failed after target became visible: bumped engine cache generation"
                );
            }
            // If activation failed before the namespace rename, the canonical
            // staging path still exists. Clean it while this transaction lock
            // is held; the journal remains sufficient for fail-closed recovery.
            staging_cleanup.cleanup_now();
            return Err(error);
        }
    };

    Ok(serde_json::json!({
        "bundle_name": req.bundle_name,
        "files_installed": files_installed,
        "total_bytes": total_bytes,
        "path": canonical_target.display().to_string(),
    }))
}

/// Reconcile one bundle transaction while the caller holds its lock. Visible
/// repairs invalidate cached engines even if this remote-install request will
/// subsequently return an already-installed result.
fn reconcile_remote_install(
    bundle_name: &str,
    state: &AppState,
    transaction: &ryeos_app::bundle_transaction::BundleTransaction,
) -> Result<Option<Value>> {
    let recovered = match transaction.reconcile(state.identity.signing_key()) {
        Ok(recovered) => recovered,
        Err(error) => {
            state.engine_cache.bump_system_install_generation();
            return Err(error).context("reconcile interrupted bundle transaction");
        }
    };
    if recovered.is_some() {
        let new_gen = state.engine_cache.bump_system_install_generation();
        tracing::info!(
            bundle = %bundle_name,
            operation = ?recovered,
            engine_cache_generation = new_gen,
            "reconciled bundle transaction: bumped engine cache generation"
        );
    }
    if matches!(
        recovered,
        Some(
            ryeos_app::bundle_transaction::BundleOperation::Install
                | ryeos_app::bundle_transaction::BundleOperation::RemoteInstall
        )
    ) && transaction.target().is_dir()
    {
        return Ok(Some(serde_json::json!({
            "bundle_name": bundle_name,
            "path": transaction.target(),
            "recovered": true,
        })));
    }
    Ok(None)
}

fn inspect_remote_fetch_heartbeat(transfer_root: &Path) -> Result<Option<std::fs::Metadata>> {
    let root_metadata = std::fs::symlink_metadata(transfer_root)
        .with_context(|| format!("inspect remote transfer root {}", transfer_root.display()))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.file_type().is_dir() {
        bail!(
            "remote transfer root {} is not a real directory",
            transfer_root.display()
        );
    }

    let heartbeat = transfer_root.join(REMOTE_FETCH_HEARTBEAT_FILE);
    let metadata = match std::fs::symlink_metadata(&heartbeat) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspect remote transfer heartbeat {}", heartbeat.display())
            })
        }
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!(
            "remote transfer heartbeat {} is not a real regular file",
            heartbeat.display()
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.uid() != root_metadata.uid() {
            bail!(
                "remote transfer heartbeat {} is not owned like its transfer root",
                heartbeat.display()
            );
        }
    }
    Ok(Some(metadata))
}

fn write_remote_fetch_heartbeat(transfer_root: &Path) -> Result<()> {
    // Validate any existing marker before atomically replacing it so a linked
    // or non-regular entry can never be treated as liveness authority.
    inspect_remote_fetch_heartbeat(transfer_root)?;
    let elapsed = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?;
    let heartbeat = transfer_root.join(REMOTE_FETCH_HEARTBEAT_FILE);
    let body = format!("{}\n", elapsed.as_nanos());
    lillux::atomic_write_private(&heartbeat, body.as_bytes()).with_context(|| {
        format!(
            "durably update remote transfer heartbeat {}",
            heartbeat.display()
        )
    })?;
    inspect_remote_fetch_heartbeat(transfer_root)?
        .context("remote transfer heartbeat disappeared after durable update")?;
    Ok(())
}

fn remove_remote_fetch_heartbeat(transfer_root: &Path) -> Result<()> {
    inspect_remote_fetch_heartbeat(transfer_root)?
        .context("remote transfer heartbeat is missing before final admission")?;
    let heartbeat = transfer_root.join(REMOTE_FETCH_HEARTBEAT_FILE);
    lillux::remove_file_durable(&heartbeat).with_context(|| {
        format!(
            "durably remove remote transfer heartbeat {}",
            heartbeat.display()
        )
    })
}

/// Remove a bounded number of request-unique transfer generations abandoned
/// by process death.
///
/// Transfers intentionally run without the registry lock, so lock ownership
/// alone cannot identify a live generation. Active transfers durably refresh a
/// verified regular-file heartbeat after every bounded object batch. Only
/// canonical UUID names that are real directories, owned like the bundles
/// root, and whose heartbeat is stale for a full day are eligible. Debris from
/// before heartbeat creation falls back to the root directory timestamp. Scan
/// and removal caps bound work under the node-wide registry lock.
fn scavenge_stale_remote_fetches(bundles_root: &Path) -> Result<()> {
    let root_metadata = match std::fs::symlink_metadata(bundles_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect bundles root {}", bundles_root.display()))
        }
    };
    if root_metadata.file_type().is_symlink() || !root_metadata.file_type().is_dir() {
        bail!(
            "bundles root {} is not a real directory",
            bundles_root.display()
        );
    }

    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt as _;
    #[cfg(unix)]
    let root_owner = root_metadata.uid();

    let mut entries = std::fs::read_dir(bundles_root)
        .with_context(|| format!("scan bundles root {}", bundles_root.display()))?
        .take(REMOTE_FETCH_SCAVENGE_SCAN_LIMIT)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);

    let now = SystemTime::now();
    let mut removed = 0usize;
    for entry in entries {
        if removed >= REMOTE_FETCH_SCAVENGE_REMOVE_LIMIT {
            break;
        }
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_remote_fetch_generation_name(&file_name) {
            continue;
        }
        let path = entry.path();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "failed to inspect stale remote bundle transfer candidate"
                );
                continue;
            }
        };
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            continue;
        }
        #[cfg(unix)]
        if metadata.uid() != root_owner {
            continue;
        }
        let modified = match inspect_remote_fetch_heartbeat(&path) {
            Ok(Some(heartbeat)) => match heartbeat.modified() {
                Ok(modified) => modified,
                Err(_) => continue,
            },
            Ok(None) => match metadata.modified() {
                Ok(modified) => modified,
                Err(_) => continue,
            },
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "refusing to scavenge remote transfer with invalid heartbeat"
                );
                continue;
            }
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age < REMOTE_FETCH_STALE_AGE {
            continue;
        }

        match lillux::remove_dir_all_durable(&path) {
            Ok(()) => {
                removed += 1;
                tracing::info!(
                    path = %path.display(),
                    age_seconds = age.as_secs(),
                    "removed abandoned remote bundle transfer generation"
                );
            }
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "failed to remove abandoned remote bundle transfer generation"
                );
            }
        }
    }
    Ok(())
}

fn is_remote_fetch_generation_name(file_name: &str) -> bool {
    let Some(rest) = file_name.strip_prefix('.') else {
        return false;
    };
    let Some((bundle_name, generation)) = rest.rsplit_once(".remote-fetch-") else {
        return false;
    };
    if crate::handlers::bundle_install::validate_name(bundle_name).is_err() {
        return false;
    }
    let Ok(generation_id) = uuid::Uuid::parse_str(generation) else {
        return false;
    };
    generation_id.hyphenated().to_string() == generation
}

fn remove_stale_transaction_staging(path: &Path) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect transaction staging {}", path.display()))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        bail!(
            "remote transaction staging {} is not a real directory",
            path.display()
        );
    }
    lillux::remove_dir_all_durable(path)
        .with_context(|| format!("remove stale transaction staging {}", path.display()))
}

/// Best-effort cancellation/error cleanup for a request-owned hidden staging
/// generation. Once a transaction rename commits, the guard is retargeted to
/// the canonical staging path and finally disarmed when that path becomes live.
struct RemoteStagingCleanup {
    path: PathBuf,
    armed: bool,
}

impl RemoteStagingCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn retarget(&mut self, path: PathBuf) {
        self.path = path;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn cleanup_now(&mut self) {
        if !self.armed {
            return;
        }
        match std::fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
                if std::fs::remove_dir_all(&self.path).is_ok() {
                    self.armed = false;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.armed = false;
            }
            _ => {}
        }
    }
}

impl Drop for RemoteStagingCleanup {
    fn drop(&mut self) {
        self.cleanup_now();
    }
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
            .objects_get_bundle_batch_with_response_limit(&hashes, REMOTE_OBJECT_RESPONSE_MAX_BYTES)
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

        // Each remote object request has a bounded total timeout. Refreshing a
        // durable marker after its fully materialized batch keeps an active
        // transfer's observed age below that bound even when the transfer root
        // itself stopped gaining top-level directory entries long ago.
        write_remote_fetch_heartbeat(local_target)
            .context("refresh remote bundle transfer heartbeat after object batch")?;
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
        if entry.path.split('/').next() == Some(REMOTE_FETCH_HEARTBEAT_FILE) {
            bail!(
                "remote bundle export entry '{}' uses the reserved transfer heartbeat path",
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
                && directories.len() + paths.len()
                    >= super::bundle_export::MAX_EXPORTED_BUNDLE_TREE_ENTRIES
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
        if directories.len() + paths.len() > super::bundle_export::MAX_EXPORTED_BUNDLE_TREE_ENTRIES
        {
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
        let mut components =
            vec!["directory"; super::super::bundle_export::MAX_EXPORTED_BUNDLE_DEPTH + 1];
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
