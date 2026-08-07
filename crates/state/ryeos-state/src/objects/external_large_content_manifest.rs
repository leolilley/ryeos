//! Content-addressed manifest for large-content-tier external content.
//!
//! The second manifest kind a realization's `manifest_hash` may resolve to.
//! The realization wire shape does not change and carries no tier
//! discriminator: bind and admission fetch this object and route on what it
//! says it is. Entries name large objects in the large-object store — contiguous
//! mmap-ready files — rather than CAS blobs, and each entry's file hash
//! commits to a fixed-size chunk list so ingest and scrub can verify
//! streaming, one chunk in memory at a time.
//!
//! Bounds here are the large tier's own: the content-tier caps live on the
//! content manifest object, and the shared realization-set wire validator
//! holds only structure plus a sanity ceiling.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const EXTERNAL_LARGE_CONTENT_MANIFEST_KIND: &str = "external_large_content_manifest";
pub const EXTERNAL_LARGE_CONTENT_SCHEMA: &str = "ryeos.external_content.large.v1";

/// Fixed chunk size for production manifests. Recorded per entry so the
/// commitment is explicit in the bytes, but v1 admits exactly this value
/// range; the constant is the default the ingest surface uses.
pub const LARGE_CONTENT_CHUNK_BYTES: u64 = 64 * 1024 * 1024;
pub const MIN_LARGE_CONTENT_CHUNK_BYTES: u64 = 1024 * 1024;
pub const MAX_LARGE_CONTENT_CHUNK_BYTES: u64 = 1024 * 1024 * 1024;

/// Per large object. Sized for real shards, enforced at ingest and re-checked
/// here so a manifest cannot smuggle a claim ingest would have refused.
pub const MAX_LARGE_CONTENT_FILE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
/// Per manifest. A model is one manifest; half a terabyte covers the largest
/// full-precision dumps this node class could hold at all.
pub const MAX_LARGE_CONTENT_TOTAL_BYTES: u64 = 512 * 1024 * 1024 * 1024;
/// Shard files, not tree entries: real models ship tens of shards.
pub const MAX_LARGE_CONTENT_MANIFEST_ENTRIES: usize = 256;
/// The chunk lists dominate: a 64 GiB entry carries 1024 chunk hashes.
pub const MAX_LARGE_CONTENT_MANIFEST_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalLargeContentManifestEntry {
    /// Mount-relative path. A file-shaped realization has exactly one entry
    /// named [`crate::objects::external_content_manifest`]'s file entry path
    /// (`content`); a tree-shaped one names its shard files.
    pub path: String,
    /// Whole-file sha256 — the large object's name in the large-object store.
    pub file_sha256: String,
    pub size: u64,
    pub chunk_size: u64,
    /// sha256 per fixed-size chunk, in order; the final chunk is short.
    pub chunk_hashes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalLargeContentManifestObject {
    pub schema: String,
    pub kind: String,
    pub entries: Vec<ExternalLargeContentManifestEntry>,
    pub entry_count: usize,
    pub total_bytes: u64,
}

impl ExternalLargeContentManifestObject {
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let manifest: Self = serde_json::from_value(value.clone())?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.kind != EXTERNAL_LARGE_CONTENT_MANIFEST_KIND {
            anyhow::bail!("unexpected external large-content manifest kind: {}", self.kind);
        }
        if self.schema != EXTERNAL_LARGE_CONTENT_SCHEMA {
            anyhow::bail!(
                "unexpected external large-content manifest schema: {}",
                self.schema
            );
        }
        if self.entry_count != self.entries.len() {
            anyhow::bail!("external large-content manifest entry count disagrees with its entries");
        }
        if self.entries.is_empty() {
            anyhow::bail!("external large-content manifest names no content");
        }
        if self.entries.len() > MAX_LARGE_CONTENT_MANIFEST_ENTRIES {
            anyhow::bail!(
                "external large-content manifest carries {} entries; the bound is {MAX_LARGE_CONTENT_MANIFEST_ENTRIES}",
                self.entries.len()
            );
        }
        let mut previous: Option<&str> = None;
        let mut summed = 0u64;
        for entry in &self.entries {
            super::validate_canonical_project_relative_path(&entry.path)?;
            if let Some(previous) = previous
                && previous.as_bytes() >= entry.path.as_bytes()
            {
                anyhow::bail!(
                    "external large-content manifest entries are not strictly ordered by path bytes"
                );
            }
            previous = Some(&entry.path);
            super::thread_snapshot::validate_canonical_hash(
                "external large-content file_sha256",
                &entry.file_sha256,
            )?;
            if entry.size == 0 {
                anyhow::bail!(
                    "external large-content entry `{}` is empty; large content is never an empty file",
                    entry.path
                );
            }
            if entry.size > MAX_LARGE_CONTENT_FILE_BYTES {
                anyhow::bail!(
                    "external large-content entry `{}` is {} bytes; the bound is {MAX_LARGE_CONTENT_FILE_BYTES}",
                    entry.path,
                    entry.size
                );
            }
            if entry.chunk_size < MIN_LARGE_CONTENT_CHUNK_BYTES
                || entry.chunk_size > MAX_LARGE_CONTENT_CHUNK_BYTES
                || !entry.chunk_size.is_power_of_two()
            {
                anyhow::bail!(
                    "external large-content entry `{}` has chunk size {}; v1 admits powers of two in \
                     [{MIN_LARGE_CONTENT_CHUNK_BYTES}, {MAX_LARGE_CONTENT_CHUNK_BYTES}]",
                    entry.path,
                    entry.chunk_size
                );
            }
            let expected_chunks = entry.size.div_ceil(entry.chunk_size);
            if entry.chunk_hashes.len() as u64 != expected_chunks {
                anyhow::bail!(
                    "external large-content entry `{}` declares {} chunk hashes for {} bytes at chunk \
                     size {}; {expected_chunks} are required",
                    entry.path,
                    entry.chunk_hashes.len(),
                    entry.size,
                    entry.chunk_size
                );
            }
            for hash in &entry.chunk_hashes {
                super::thread_snapshot::validate_canonical_hash(
                    "external large-content chunk hash",
                    hash,
                )?;
            }
            summed = summed
                .checked_add(entry.size)
                .ok_or_else(|| anyhow::anyhow!("external large-content manifest sizes overflow"))?;
        }
        if summed != self.total_bytes {
            anyhow::bail!(
                "external large-content manifest declares {} total bytes but its entries sum to {summed}",
                self.total_bytes
            );
        }
        if self.total_bytes > MAX_LARGE_CONTENT_TOTAL_BYTES {
            anyhow::bail!(
                "external large-content manifest declares {} bytes; the bound is {MAX_LARGE_CONTENT_TOTAL_BYTES}",
                self.total_bytes
            );
        }
        let canonical = lillux::canonical_json(&serde_json::to_value(self)?)?;
        if canonical.len() > MAX_LARGE_CONTENT_MANIFEST_BYTES {
            anyhow::bail!(
                "external large-content manifest is {} bytes; the bound is {MAX_LARGE_CONTENT_MANIFEST_BYTES}",
                canonical.len()
            );
        }
        Ok(())
    }

    /// Every large object this manifest binds — the large-object store's roots
    /// for reachability, exactly as `referenced_blobs` is for content.
    pub fn referenced_large_objects(&self) -> Vec<String> {
        let mut objects = BTreeSet::new();
        for entry in &self.entries {
            objects.insert(entry.file_sha256.clone());
        }
        objects.into_iter().collect()
    }

    /// Whether this manifest can back a file-shaped realization: exactly one
    /// entry at the file realization's fixed entry path.
    pub fn is_file_shaped(&self) -> bool {
        self.entries.len() == 1
            && self.entries[0].path == super::external_content_manifest::FILE_REALIZATION_ENTRY_PATH
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, size: u64) -> ExternalLargeContentManifestEntry {
        let chunk_size = MIN_LARGE_CONTENT_CHUNK_BYTES;
        let chunks = size.div_ceil(chunk_size);
        ExternalLargeContentManifestEntry {
            path: path.to_string(),
            file_sha256: "a".repeat(64),
            size,
            chunk_size,
            chunk_hashes: (0..chunks).map(|_| "b".repeat(64)).collect(),
        }
    }

    fn manifest(entries: Vec<ExternalLargeContentManifestEntry>) -> ExternalLargeContentManifestObject {
        let total = entries.iter().map(|entry| entry.size).sum();
        ExternalLargeContentManifestObject {
            schema: EXTERNAL_LARGE_CONTENT_SCHEMA.to_string(),
            kind: EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.to_string(),
            entry_count: entries.len(),
            entries,
            total_bytes: total,
        }
    }

    #[test]
    fn a_sharded_manifest_round_trips() {
        let object = manifest(vec![
            entry("model-00001.safetensors", 3 * MIN_LARGE_CONTENT_CHUNK_BYTES + 7),
            entry("model-00002.safetensors", MIN_LARGE_CONTENT_CHUNK_BYTES),
        ]);
        let value = object.to_value().unwrap();
        assert_eq!(ExternalLargeContentManifestObject::from_value(&value).unwrap(), object);
        assert!(!object.is_file_shaped());
        assert_eq!(object.referenced_large_objects(), vec!["a".repeat(64)]);
    }

    #[test]
    fn a_single_content_entry_is_file_shaped() {
        let object = manifest(vec![entry(
            crate::objects::FILE_REALIZATION_ENTRY_PATH,
            1,
        )]);
        object.validate().unwrap();
        assert!(object.is_file_shaped());
    }

    #[test]
    fn the_chunk_list_must_account_for_every_byte() {
        let mut object = manifest(vec![entry("model.safetensors", 2 * MIN_LARGE_CONTENT_CHUNK_BYTES)]);
        object.entries[0].chunk_hashes.pop();
        let error = object.validate().unwrap_err().to_string();
        assert!(error.contains("are required"), "got: {error}");
    }

    #[test]
    fn empty_content_and_oversized_claims_are_refused() {
        let object = manifest(vec![entry("model.safetensors", 0)]);
        object.entries[0].chunk_hashes.is_empty();
        assert!(object.validate().unwrap_err().to_string().contains("never an empty file"));

        let mut oversized = entry("model.safetensors", MAX_LARGE_CONTENT_FILE_BYTES);
        oversized.size = MAX_LARGE_CONTENT_FILE_BYTES + 1;
        oversized.chunk_hashes =
            (0..oversized.size.div_ceil(oversized.chunk_size)).map(|_| "b".repeat(64)).collect();
        // Chunk-list bytes for a >64 GiB claim blow the manifest byte bound
        // before the per-file bound answers; both refusals are fail-closed.
        assert!(manifest(vec![oversized]).validate().is_err());
    }

    #[test]
    fn chunk_size_is_bounded_to_the_v1_range() {
        let mut object = manifest(vec![entry("model.safetensors", 8)]);
        object.entries[0].chunk_size = 512;
        object.entries[0].chunk_hashes = vec!["b".repeat(64)];
        let error = object.validate().unwrap_err().to_string();
        assert!(error.contains("powers of two"), "got: {error}");
    }

    #[test]
    fn entries_stay_strictly_ordered_by_path_bytes() {
        let object = manifest(vec![entry("b.safetensors", 1), entry("a.safetensors", 1)]);
        let error = object.validate().unwrap_err().to_string();
        assert!(error.contains("strictly ordered"), "got: {error}");
    }
}
