//! `bundle/export` — export an installed bundle as CAS objects.
//!
//! Walks the bundle directory tree, ingests every file into the node's
//! CAS, and returns a manifest of all file hashes. The calling node
//! can then fetch those objects via `objects_get`.

use std::sync::Arc;
use std::{fs::File, io::Read as _};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

pub(crate) const EXPORTED_BUNDLE_ENTRY_KIND_FILE: &str = "file";
pub(crate) const MAX_EXPORTED_BUNDLE_FILES: usize = 10_000;
pub(crate) const MAX_EXPORTED_BUNDLE_TREE_ENTRIES: usize = 20_000;
pub(crate) const MAX_EXPORTED_BUNDLE_DEPTH: usize = 64;
pub(crate) const MAX_EXPORTED_BUNDLE_PATH_BYTES: usize = 4 * 1024;
pub(crate) const MAX_EXPORTED_BUNDLE_FILE_BYTES: u64 = 32 * 1024 * 1024;
pub(crate) const MAX_EXPORTED_BUNDLE_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Bundle name to export.
    pub bundle_name: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value, HandlerError> {
    // Validate bundle name.
    super::bundle_install::validate_name(&req.bundle_name)
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;

    // Find the bundle path from node config.
    let bundle_path = state
        .node_config
        .bundles
        .iter()
        .find(|b| b.name == req.bundle_name)
        .map(|b| b.path.clone())
        .ok_or(HandlerError::NotFound)?;

    let bundle_metadata = std::fs::symlink_metadata(&bundle_path).map_err(|error| {
        HandlerError::Internal(format!(
            "read bundle path metadata {}: {error}",
            bundle_path.display()
        ))
    })?;
    if bundle_metadata.file_type().is_symlink() || !bundle_metadata.file_type().is_dir() {
        return Err(HandlerError::Internal(format!(
            "bundle path {} is not a real directory",
            bundle_path.display()
        )));
    }

    let cas_root = state
        .state_store
        .cas_root()
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
    let cas = lillux::cas::CasStore::new(cas_root);

    // Walk the bundle tree, ingest each file into CAS.
    let mut entries: Vec<Value> = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut tree_entries = 0usize;
    walk_and_ingest(
        &bundle_path,
        &bundle_path,
        &cas,
        &mut entries,
        &mut total_bytes,
        &mut tree_entries,
        0,
    )
    .map_err(|e| HandlerError::Internal(e.to_string()))?;

    Ok(serde_json::json!({
        "bundle_name": req.bundle_name,
        "bundle_path": bundle_path.display().to_string(),
        "file_count": entries.len(),
        "total_bytes": total_bytes,
        "entries": entries,
    }))
}

/// Recursively walk a directory, store each regular file as a blob in CAS.
fn walk_and_ingest(
    base: &std::path::Path,
    current: &std::path::Path,
    cas: &lillux::cas::CasStore,
    entries: &mut Vec<Value>,
    total_bytes: &mut u64,
    tree_entries: &mut usize,
    depth: usize,
) -> Result<()> {
    if depth > MAX_EXPORTED_BUNDLE_DEPTH {
        bail!(
            "bundle export tree exceeds maximum depth of {} below {}",
            MAX_EXPORTED_BUNDLE_DEPTH,
            base.display()
        );
    }
    for entry in
        std::fs::read_dir(current).with_context(|| format!("read dir {}", current.display()))?
    {
        let entry = entry?;
        *tree_entries = tree_entries
            .checked_add(1)
            .context("bundle export tree entry count overflow")?;
        if *tree_entries > MAX_EXPORTED_BUNDLE_TREE_ENTRIES {
            bail!(
                "bundle export tree exceeds maximum of {} files/directories",
                MAX_EXPORTED_BUNDLE_TREE_ENTRIES
            );
        }
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("read metadata {}", path.display()))?;
        let file_type = metadata.file_type();
        let rel = path
            .strip_prefix(base)
            .context("strip_prefix")?
            .to_str()
            .with_context(|| format!("bundle entry path is not valid UTF-8: {}", path.display()))?
            .to_string();
        if rel.len() > MAX_EXPORTED_BUNDLE_PATH_BYTES {
            bail!(
                "bundle export entry path exceeds {} bytes: {}",
                MAX_EXPORTED_BUNDLE_PATH_BYTES,
                rel
            );
        }

        if file_type.is_symlink() {
            bail!(
                "bundle export refuses symlink entry '{}' at {}",
                rel,
                path.display()
            );
        }
        if file_type.is_dir() {
            walk_and_ingest(
                base,
                &path,
                cas,
                entries,
                total_bytes,
                tree_entries,
                depth + 1,
            )?;
        } else if file_type.is_file() {
            if entries.len() >= MAX_EXPORTED_BUNDLE_FILES {
                bail!(
                    "bundle export exceeds maximum of {} regular files",
                    MAX_EXPORTED_BUNDLE_FILES
                );
            }
            if metadata.len() > MAX_EXPORTED_BUNDLE_FILE_BYTES {
                bail!(
                    "bundle export file '{}' is {} bytes; maximum is {}",
                    rel,
                    metadata.len(),
                    MAX_EXPORTED_BUNDLE_FILE_BYTES
                );
            }
            let mode = validated_unix_file_mode(&metadata, &path)?;
            let capacity = usize::try_from(metadata.len())
                .context("bundle file size does not fit in memory address space")?;
            let mut bytes = Vec::with_capacity(capacity);
            File::open(&path)
                .with_context(|| format!("open file {}", path.display()))?
                .take(MAX_EXPORTED_BUNDLE_FILE_BYTES + 1)
                .read_to_end(&mut bytes)
                .with_context(|| format!("read file {}", path.display()))?;
            let actual_size = u64::try_from(bytes.len()).context("bundle file size exceeds u64")?;
            if actual_size > MAX_EXPORTED_BUNDLE_FILE_BYTES {
                bail!(
                    "bundle export file '{}' grew beyond maximum of {} bytes while reading",
                    rel,
                    MAX_EXPORTED_BUNDLE_FILE_BYTES
                );
            }
            *total_bytes = (*total_bytes)
                .checked_add(actual_size)
                .context("bundle export total size overflow")?;
            if *total_bytes > MAX_EXPORTED_BUNDLE_TOTAL_BYTES {
                bail!(
                    "bundle export exceeds maximum total size of {} bytes",
                    MAX_EXPORTED_BUNDLE_TOTAL_BYTES
                );
            }
            let hash = cas.store_blob(&bytes)?;
            entries.push(serde_json::json!({
                "kind": EXPORTED_BUNDLE_ENTRY_KIND_FILE,
                "path": rel,
                "hash": hash,
                "size": actual_size,
                "mode": mode,
            }));
        } else {
            bail!(
                "bundle export refuses non-regular entry '{}' at {}",
                rel,
                path.display()
            );
        }
    }
    Ok(())
}

fn validated_unix_file_mode(metadata: &std::fs::Metadata, path: &std::path::Path) -> Result<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let mode = metadata.permissions().mode() & 0o7777;
        if mode & !0o777 != 0 {
            bail!(
                "bundle export refuses special permission bits on {} ({mode:#o})",
                path.display()
            );
        }
        Ok(mode)
    }
    #[cfg(not(unix))]
    {
        let _ = (metadata, path);
        bail!("bundle export requires Unix file-mode support")
    }
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle/export",
    endpoint: "bundle.export",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle/export"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await.map_err(Into::into)
        })
    },
};
