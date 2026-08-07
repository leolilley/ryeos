//! Daemon-mediated large-content ingest.
//!
//! The large-object store lives under the pinned state authority, so bytes
//! enter it only through the daemon, and only for callers whose signed
//! bundle manifest declares `runtime_authority.large_content: [ingest]` and
//! whose item requests it — authority minted from data, never composed. The
//! service streams each source file into the store (resumable, chunk-
//! verified), publishes the `external_large_content_manifest` object into
//! CAS under the mutation guard, and answers with the pin an author pastes
//! into a declaration. What the bytes mean is the caller's business; this
//! surface never asks.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use ryeos_bundle::manifest::LargeContentOperation;
use ryeos_bundle::runtime_authority::large_content_cap;
use ryeos_runtime::authorizer::{AuthorizationPolicy, Authorizer};
use ryeos_state::objects::{
    EXTERNAL_LARGE_CONTENT_MANIFEST_KIND, EXTERNAL_LARGE_CONTENT_SCHEMA,
    ExternalLargeContentManifestEntry, ExternalLargeContentManifestObject,
    FILE_REALIZATION_ENTRY_PATH, MAX_LARGE_CONTENT_MANIFEST_ENTRIES,
};

use crate::callback_token::CallbackCapability;
use crate::state_store::StateStore;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLargeContentIngestParams {
    /// Calling thread, injected by the callback client; authorization rides
    /// the callback capability, not this label.
    pub thread_id: String,
    /// Absolute path to a regular file or a directory of regular files.
    pub source_path: String,
    /// Optional pinned expectation for a single-file source: mismatch
    /// refuses publication instead of minting a surprise identity.
    #[serde(default)]
    pub expected_sha256: Option<String>,
}

#[derive(Debug, Serialize)]
struct IngestedEntryReport {
    path: String,
    file_sha256: String,
    size: u64,
    chunk_count: usize,
    deduplicated: bool,
    resumed_bytes: u64,
}

pub struct RuntimeLargeContentService;

impl RuntimeLargeContentService {
    pub fn ingest(
        state_store: &StateStore,
        authorizer: &Authorizer,
        cap: &CallbackCapability,
        params: RuntimeLargeContentIngestParams,
    ) -> Result<Value> {
        let required = large_content_cap(&LargeContentOperation::Ingest);
        authorizer
            .authorize(&cap.effective_caps, &AuthorizationPolicy::require(&required))
            .with_context(|| {
                format!(
                    "missing required capability: {required} — large-content ingest is manifest \
                     runtime authority: declare `runtime_authority.large_content: [ingest]` in \
                     the bundle's `.ai/manifest.source.yaml` and request it from the item under \
                     `requires.capabilities.manifest.runtime_authority`"
                )
            })?;

        let source = PathBuf::from(&params.source_path);
        if !source.is_absolute() {
            bail!("large-content source path must be absolute: {}", source.display());
        }
        let metadata = std::fs::symlink_metadata(&source)
            .with_context(|| format!("reading large-content source {}", source.display()))?;

        let sources: Vec<(String, PathBuf)> = if metadata.is_file() {
            vec![(FILE_REALIZATION_ENTRY_PATH.to_string(), source.clone())]
        } else if metadata.is_dir() {
            if params.expected_sha256.is_some() {
                bail!("expected_sha256 applies to a single-file source, not a directory");
            }
            collect_directory_sources(&source)?
        } else {
            bail!(
                "large-content source {} is neither a regular file nor a directory",
                source.display()
            );
        };
        if sources.is_empty() {
            bail!("large-content source {} contains no regular files", source.display());
        }
        if sources.len() > MAX_LARGE_CONTENT_MANIFEST_ENTRIES {
            bail!(
                "large-content source {} has {} files; the manifest bound is \
                 {MAX_LARGE_CONTENT_MANIFEST_ENTRIES}",
                source.display(),
                sources.len()
            );
        }

        let authority = state_store.with_state_db(|db| db.pinned_authority())?;
        let guard = authority.acquire_shared_guard()?;
        authority.ensure_guard(&guard)?;
        let store = authority.large_object_store()?;

        let mut entries = Vec::with_capacity(sources.len());
        let mut reports = Vec::with_capacity(sources.len());
        let mut total_bytes = 0u64;
        for (entry_path, file_path) in &sources {
            let ingested = store
                .ingest_from_path(file_path, params.expected_sha256.as_deref())
                .with_context(|| format!("ingesting {}", file_path.display()))?;
            total_bytes = total_bytes
                .checked_add(ingested.size)
                .ok_or_else(|| anyhow::anyhow!("large-content ingest byte total overflow"))?;
            reports.push(IngestedEntryReport {
                path: entry_path.clone(),
                file_sha256: ingested.file_sha256.clone(),
                size: ingested.size,
                chunk_count: ingested.chunk_hashes.len(),
                deduplicated: ingested.deduplicated,
                resumed_bytes: ingested.resumed_bytes,
            });
            entries.push(ExternalLargeContentManifestEntry {
                path: entry_path.clone(),
                file_sha256: ingested.file_sha256,
                size: ingested.size,
                chunk_size: ingested.chunk_size,
                chunk_hashes: ingested.chunk_hashes,
            });
        }

        let manifest = ExternalLargeContentManifestObject {
            schema: EXTERNAL_LARGE_CONTENT_SCHEMA.to_string(),
            kind: EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.to_string(),
            entry_count: entries.len(),
            entries,
            total_bytes,
        };
        let manifest_value = manifest.to_value()?;
        let manifest_hash = authority
            .cas_store()?
            .store_object(&manifest_value)
            .context("publishing large-content manifest object")?;
        authority.ensure_guard(&guard)?;
        drop(guard);

        let shape = if manifest.is_file_shaped() { "file" } else { "tree" };
        Ok(json!({
            "manifest_hash": &manifest_hash,
            "entry_count": manifest.entry_count,
            "total_bytes": manifest.total_bytes,
            "entries": serde_json::to_value(&reports)?,
            "pin": {
                "kind": shape,
                "mode": "pinned",
                "digest": &manifest_hash,
            },
        }))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLargeContentScrubParams {
    pub thread_id: String,
}

impl RuntimeLargeContentService {
    /// Re-verify every stored large object streaming, chunk by chunk, and
    /// report typed integrity findings. A non-empty findings list is
    /// substrate damage, not noise.
    pub fn scrub(
        state_store: &StateStore,
        authorizer: &Authorizer,
        cap: &CallbackCapability,
        _params: RuntimeLargeContentScrubParams,
    ) -> Result<Value> {
        let required = large_content_cap(&LargeContentOperation::Scrub);
        authorizer
            .authorize(&cap.effective_caps, &AuthorizationPolicy::require(&required))
            .with_context(|| {
                format!(
                    "missing required capability: {required} — large-content scrub is manifest \
                     runtime authority: declare `runtime_authority.large_content: [scrub]` in \
                     the bundle's `.ai/manifest.source.yaml` and request it from the item"
                )
            })?;
        let authority = state_store.with_state_db(|db| db.pinned_authority())?;
        let store = authority.large_object_store()?;
        let report = store.scrub_all()?;
        let staging_reclaimed = store.sweep_abandoned_staging()?;
        Ok(json!({
            "objects_verified": report.objects_verified,
            "bytes_verified": report.bytes_verified,
            "findings": serde_json::to_value(&report.findings)?,
            "staging_reclaimed": staging_reclaimed,
        }))
    }
}

/// Walk a shard directory into (manifest path, filesystem path) pairs,
/// sorted by path bytes as the manifest requires. Symlinks fail closed: a
/// pinned identity must not depend on where a link points today.
fn collect_directory_sources(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut sources = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("reading large-content directory {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                bail!(
                    "large-content source contains a symlink: {} — pinned content must be \
                     resolved bytes",
                    path.display()
                );
            }
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .expect("walked path is under its root")
                    .to_str()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "large-content source path is not UTF-8: {}",
                            path.display()
                        )
                    })?
                    .to_string();
                ryeos_state::objects::validate_canonical_project_relative_path(&relative)
                    .with_context(|| format!("large-content entry path `{relative}`"))?;
                sources.push((relative, path));
            } else {
                bail!(
                    "large-content source contains a special file: {}",
                    path.display()
                );
            }
        }
    }
    sources.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    Ok(sources)
}
