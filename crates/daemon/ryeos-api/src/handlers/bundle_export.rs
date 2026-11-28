//! `bundle/export` — export an installed bundle as CAS objects.
//!
//! Walks the bundle directory tree, ingests every file into the node's
//! CAS, and returns a manifest of all file hashes. The calling node
//! can then fetch those objects via `objects_get`.

use std::sync::Arc;
use std::{fs::File, io::Read as _};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

pub(crate) const EXPORTED_BUNDLE_ENTRY_KIND_FILE: &str = "file";
/// Format version of the bundle export answer. Version 2 records each file's
/// mode as exactly 0o644 or 0o755 (the executable bit is the only content);
/// version 1 recorded the host's raw modes and is refused, never read.
pub(crate) const BUNDLE_EXPORT_FORMAT: &str = "ryeos.bundle-export/v2";
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
        .map_err(|e| HandlerError::BadRequest(format!("{e:#}")))?;

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
            "read installed bundle `{}`: {error}",
            req.bundle_name
        ))
    })?;
    if bundle_metadata.file_type().is_symlink() || !bundle_metadata.file_type().is_dir() {
        return Err(HandlerError::Internal(format!(
            "installed bundle `{}` is not a real directory",
            req.bundle_name
        )));
    }

    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
    let _cas_guard = authority
        .acquire_shared_guard()
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
    let cas = authority
        .cas_store()
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| {
            HandlerError::Internal(format!("cannot acquire CAS write permit: {error:#}"))
        })?;

    // Walk the bundle tree, ingest each file into CAS.
    let mut entries: Vec<Value> = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut tree_entries = 0usize;
    walk_and_ingest(
        &bundle_path,
        &bundle_path,
        Some(&cas),
        &mut entries,
        &mut total_bytes,
        &mut tree_entries,
        0,
    )
    .map_err(|error| {
        // The walk's errors name host paths; they stay in the daemon log.
        tracing::error!(bundle = %req.bundle_name, error = %format!("{error:#}"), "bundle export failed");
        HandlerError::Internal(format!("export of installed bundle `{}` failed", req.bundle_name))
    })?;

    remember_exported_blobs(&state, &req.bundle_name, &entries);

    Ok(serde_json::json!({
        "format": BUNDLE_EXPORT_FORMAT,
        "bundle_name": req.bundle_name,
        "file_count": entries.len(),
        "total_bytes": total_bytes,
        "entries": entries,
    }))
}

/// Blob hashes of each bundle's most recent export, keyed by store and bundle.
/// Root-scoped reads of a `bundle_export` root are served from this set: the
/// export is what names the files a caller may then fetch.
fn exported_blob_cache()
-> &'static std::sync::Mutex<std::collections::HashMap<String, std::collections::BTreeSet<String>>>
{
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::collections::BTreeSet<String>>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn exported_blob_key(state: &AppState, bundle_name: &str) -> String {
    format!("{}|{bundle_name}", state.config.app_root.display())
}

fn remember_exported_blobs(state: &AppState, bundle_name: &str, entries: &[Value]) {
    let hashes = entries
        .iter()
        .filter_map(|entry| entry.get("hash").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    if let Ok(mut cache) = exported_blob_cache().lock() {
        cache.insert(exported_blob_key(state, bundle_name), hashes);
    }
}

/// The file blobs an installed bundle exports. Served from the last export
/// when there is one; otherwise the installed tree is hashed without being
/// ingested.
pub(crate) fn exported_blob_hashes(
    state: &AppState,
    bundle_name: &str,
) -> Result<std::collections::BTreeSet<String>> {
    let key = exported_blob_key(state, bundle_name);
    if let Some(hashes) = exported_blob_cache()
        .lock()
        .map_err(|_| anyhow::anyhow!("bundle export cache poisoned"))?
        .get(&key)
    {
        return Ok(hashes.clone());
    }
    super::bundle_install::validate_name(bundle_name)?;
    let bundle_path = state
        .node_config
        .bundles
        .iter()
        .find(|b| b.name == bundle_name)
        .map(|b| b.path.clone())
        .context("bundle is not installed")?;
    let mut entries = Vec::new();
    let mut total_bytes = 0;
    let mut tree_entries = 0;
    walk_and_ingest(
        &bundle_path,
        &bundle_path,
        None,
        &mut entries,
        &mut total_bytes,
        &mut tree_entries,
        0,
    )?;
    remember_exported_blobs(state, bundle_name, &entries);
    Ok(entries
        .iter()
        .filter_map(|entry| entry.get("hash").and_then(Value::as_str))
        .map(str::to_owned)
        .collect())
}

/// Recursively walk a directory, store each regular file as a blob in CAS.
pub(crate) fn walk_and_ingest(
    base: &std::path::Path,
    current: &std::path::Path,
    cas: Option<&lillux::cas::CasStore>,
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
            let mode = normalised_export_mode(&path)?;
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
            let hash = match cas {
                Some(cas) => cas.store_blob(&bytes)?,
                None => lillux::sha256_hex(&bytes),
            };
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

/// The mode a file is exported with: 0o755 when the source has any execute
/// bit, else 0o644. The host's umask and group or other bits never reach the
/// export, so two hosts holding the same files export the same bytes.
fn normalised_export_mode(path: &std::path::Path) -> Result<u32> {
    let observed = lillux::observe_tree_entry_no_follow(path)?;
    if observed.kind != lillux::TreeEntryKind::RegularFile {
        bail!(
            "bundle export refuses non-regular entry at {}",
            path.display()
        );
    }
    Ok(ryeos_engine::trust::normalised_file_mode(observed.mode))
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

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::tree_entry::test_support::{make_fifo, make_symlink, set_mode};

    fn tree(file_mode: u32, exec_mode: u32, dir_mode: u32) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".ai/sub")).unwrap();
        std::fs::write(root.join(".ai/a.txt"), b"alpha").unwrap();
        std::fs::write(root.join(".ai/sub/run.sh"), b"#!/bin/sh\n").unwrap();
        set_mode(&root.join(".ai/a.txt"), file_mode).unwrap();
        set_mode(&root.join(".ai/sub/run.sh"), exec_mode).unwrap();
        set_mode(&root.join(".ai/sub"), dir_mode).unwrap();
        set_mode(&root.join(".ai"), dir_mode).unwrap();
        dir
    }

    fn walk(root: &std::path::Path) -> Result<Vec<Value>> {
        let (mut entries, mut total, mut count) = (Vec::new(), 0, 0);
        walk_and_ingest(root, root, None, &mut entries, &mut total, &mut count, 0)?;
        entries.sort_by_key(|entry| entry["path"].as_str().unwrap().to_owned());
        Ok(entries)
    }

    #[test]
    fn two_hosts_with_different_umasks_export_the_same_entries() {
        let strict = tree(0o644, 0o755, 0o755);
        let loose = tree(0o664, 0o775, 0o775);
        let odd = tree(0o600, 0o700, 0o700);
        let a = walk(strict.path()).unwrap();
        assert_eq!(a, walk(loose.path()).unwrap());
        assert_eq!(a, walk(odd.path()).unwrap());
        let mode_of = |path: &str| {
            a.iter().find(|entry| entry["path"] == path).unwrap()["mode"]
                .as_u64()
                .unwrap()
        };
        assert_eq!(mode_of(".ai/a.txt"), 0o644);
        assert_eq!(mode_of(".ai/sub/run.sh"), 0o755);
    }

    #[test]
    fn an_export_refuses_a_symlink_or_a_special_file() {
        let linked = tree(0o644, 0o755, 0o755);
        make_symlink(
            std::path::Path::new("a.txt"),
            &linked.path().join(".ai/link"),
        )
        .unwrap();
        assert!(walk(linked.path()).is_err());

        let piped = tree(0o644, 0o755, 0o755);
        make_fifo(&piped.path().join(".ai/pipe")).unwrap();
        assert!(walk(piped.path()).is_err());
    }
}
