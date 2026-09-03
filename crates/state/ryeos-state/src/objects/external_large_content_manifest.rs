//! Content-addressed manifest for large-content-tier external content.
//!
//! The second manifest kind a realization's `manifest_hash` may resolve to.
//! The realization wire shape does not change and carries no tier
//! discriminator: bind and admission fetch this object and route on what it
//! says it is. A tree remains a tree: directories, normalized file modes, and
//! symlinks are part of its identity. Small files stay in CAS; files above
//! the content-tier ceiling name contiguous mmap-ready
//! objects in the large-object store. Each large file commits to a fixed-size
//! chunk list so ingest and scrub can verify it streaming, one chunk in memory
//! at a time.
//!
//! Bounds here are the large tier's own: the content-tier caps live on the
//! content manifest object, and the shared realization-set wire validator
//! holds only structure plus a sanity ceiling.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const EXTERNAL_LARGE_CONTENT_MANIFEST_KIND: &str = "external_large_content_manifest";
pub const EXTERNAL_LARGE_CONTENT_SCHEMA: &str = "ryeos.external_content.large.v2";

/// Fixed chunk size for production manifests. Recorded per entry so the
/// commitment is explicit in the bytes. The current schema admits this value
/// range; the constant is the default the ingest surface uses.
pub const LARGE_CONTENT_CHUNK_BYTES: u64 = 64 * 1024 * 1024;
pub const MIN_LARGE_CONTENT_CHUNK_BYTES: u64 = 1024 * 1024;
pub const MAX_LARGE_CONTENT_CHUNK_BYTES: u64 = 1024 * 1024 * 1024;

/// Per large object, enforced at ingest and re-checked here so a manifest
/// cannot smuggle a claim ingest would have refused.
pub const MAX_LARGE_CONTENT_FILE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
/// Per manifest. The ceiling permits large retained realizations without
/// making the substrate unbounded.
pub const MAX_LARGE_CONTENT_TOTAL_BYTES: u64 = 512 * 1024 * 1024 * 1024;
/// Regular files, not tree entries.
pub const MAX_LARGE_CONTENT_MANIFEST_ENTRIES: usize = 256;
/// The chunk lists dominate: a 64 GiB entry carries 1024 chunk hashes.
pub const MAX_LARGE_CONTENT_MANIFEST_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalLargeContentManifestEntry {
    /// Mount-relative path. A file-shaped realization has exactly one regular
    /// file entry named [`crate::objects::external_content_manifest`]'s file
    /// entry path (`content`).
    pub path: String,
    pub kind: super::ExternalContentManifestEntryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// Content-tier blob for a bounded file. Exactly one of `blob_hash` and
    /// `file_sha256` is present on a file entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_hash: Option<String>,
    /// Whole-file sha256 — the large object's name in the large-object store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_size: Option<u64>,
    /// sha256 per fixed-size chunk, in order; the final chunk is short. Empty
    /// for content-tier files and non-file entries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chunk_hashes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
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
            anyhow::bail!(
                "unexpected external large-content manifest kind: {}",
                self.kind
            );
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
            match entry.kind {
                super::ExternalContentManifestEntryKind::File => {
                    let size = entry.size.ok_or_else(|| {
                        anyhow::anyhow!("external large-content file `{}` has no size", entry.path)
                    })?;
                    if size > MAX_LARGE_CONTENT_FILE_BYTES {
                        anyhow::bail!(
                            "external large-content entry `{}` is {size} bytes; the bound is {MAX_LARGE_CONTENT_FILE_BYTES}",
                            entry.path
                        );
                    }
                    if !matches!(entry.mode, Some(0o644) | Some(0o755)) {
                        anyhow::bail!(
                            "external large-content file `{}` has an invalid normalized mode",
                            entry.path
                        );
                    }
                    if entry.target.is_some() {
                        anyhow::bail!(
                            "external large-content file `{}` carries a link target",
                            entry.path
                        );
                    }
                    match (entry.blob_hash.as_deref(), entry.file_sha256.as_deref()) {
                        (Some(blob_hash), None) => {
                            super::thread_snapshot::validate_canonical_hash(
                                "external large-content blob_hash",
                                blob_hash,
                            )?;
                            if size > super::MAX_EXTERNAL_CONTENT_FILE_BYTES {
                                anyhow::bail!(
                                    "external large-content blob entry `{}` exceeds the content-tier file bound",
                                    entry.path
                                );
                            }
                            if entry.chunk_size.is_some() || !entry.chunk_hashes.is_empty() {
                                anyhow::bail!(
                                    "external large-content blob entry `{}` carries large-object chunks",
                                    entry.path
                                );
                            }
                        }
                        (None, Some(file_sha256)) => {
                            super::thread_snapshot::validate_canonical_hash(
                                "external large-content file_sha256",
                                file_sha256,
                            )?;
                            if size == 0 {
                                anyhow::bail!(
                                    "external large-content object `{}` is empty; empty files belong in CAS",
                                    entry.path
                                );
                            }
                            let chunk_size = entry.chunk_size.ok_or_else(|| {
                                anyhow::anyhow!(
                                    "external large-content object `{}` has no chunk size",
                                    entry.path
                                )
                            })?;
                            if chunk_size < MIN_LARGE_CONTENT_CHUNK_BYTES
                                || chunk_size > MAX_LARGE_CONTENT_CHUNK_BYTES
                                || !chunk_size.is_power_of_two()
                            {
                                anyhow::bail!(
                                    "external large-content entry `{}` has chunk size {chunk_size}; the current schema admits powers of two in \
                                     [{MIN_LARGE_CONTENT_CHUNK_BYTES}, {MAX_LARGE_CONTENT_CHUNK_BYTES}]",
                                    entry.path
                                );
                            }
                            let expected_chunks = size.div_ceil(chunk_size);
                            if entry.chunk_hashes.len() as u64 != expected_chunks {
                                anyhow::bail!(
                                    "external large-content entry `{}` declares {} chunk hashes for {size} bytes at chunk \
                                     size {chunk_size}; {expected_chunks} are required",
                                    entry.path,
                                    entry.chunk_hashes.len()
                                );
                            }
                            for hash in &entry.chunk_hashes {
                                super::thread_snapshot::validate_canonical_hash(
                                    "external large-content chunk hash",
                                    hash,
                                )?;
                            }
                        }
                        (Some(_), Some(_)) => anyhow::bail!(
                            "external large-content file `{}` names both CAS and large-object bytes",
                            entry.path
                        ),
                        (None, None) => anyhow::bail!(
                            "external large-content file `{}` names no bytes",
                            entry.path
                        ),
                    }
                    summed = summed.checked_add(size).ok_or_else(|| {
                        anyhow::anyhow!("external large-content manifest sizes overflow")
                    })?;
                }
                super::ExternalContentManifestEntryKind::Dir => {
                    if entry.mode.is_some()
                        || entry.blob_hash.is_some()
                        || entry.file_sha256.is_some()
                        || entry.size.is_some()
                        || entry.chunk_size.is_some()
                        || !entry.chunk_hashes.is_empty()
                        || entry.target.is_some()
                    {
                        anyhow::bail!(
                            "external large-content directory `{}` carries content",
                            entry.path
                        );
                    }
                }
                super::ExternalContentManifestEntryKind::Symlink => {
                    if entry.mode.is_some()
                        || entry.blob_hash.is_some()
                        || entry.file_sha256.is_some()
                        || entry.size.is_some()
                        || entry.chunk_size.is_some()
                        || !entry.chunk_hashes.is_empty()
                    {
                        anyhow::bail!(
                            "external large-content symlink `{}` carries file content",
                            entry.path
                        );
                    }
                    match entry.target.as_deref() {
                        Some(target)
                            if !target.is_empty()
                                && !target.as_bytes().contains(&0)
                                && target.len() <= super::MAX_INLINE_SYMLINK_TARGET_BYTES =>
                        {
                            super::validate_internal_symlink_target(
                                &entry.path,
                                target.as_bytes(),
                            )?;
                        }
                        _ => anyhow::bail!(
                            "external large-content symlink `{}` has no valid target",
                            entry.path
                        ),
                    }
                }
            }
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
        super::validate_internal_symlink_graph(self.entries.iter().filter_map(|entry| {
            (entry.kind == super::ExternalContentManifestEntryKind::Symlink).then(|| {
                (
                    entry.path.as_str(),
                    entry.target.as_deref().expect("validated target"),
                )
            })
        }))?;
        super::external_content_manifest::validate_manifest_tree_namespace(
            self.entries
                .iter()
                .map(|entry| (entry.path.as_str(), entry.kind)),
        )?;
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
            if let Some(hash) = &entry.file_sha256 {
                objects.insert(hash.clone());
            }
        }
        objects.into_iter().collect()
    }

    pub fn referenced_blobs(&self) -> Vec<String> {
        let mut blobs = BTreeSet::new();
        for entry in &self.entries {
            if let Some(hash) = &entry.blob_hash {
                blobs.insert(hash.clone());
            }
        }
        blobs.into_iter().collect()
    }

    /// Whether this manifest can back a file-shaped realization: exactly one
    /// entry at the file realization's fixed entry path.
    pub fn is_file_shaped(&self) -> bool {
        self.entries.len() == 1
            && self.entries[0].path == super::external_content_manifest::FILE_REALIZATION_ENTRY_PATH
            && self.entries[0].kind == super::ExternalContentManifestEntryKind::File
    }
}

/// Load a CAS object only when it identifies the current large-content
/// manifest kind. Absence or a different registered object kind is `None`;
/// malformed bytes claiming this kind are an integrity error.
pub fn load_if_large_content_manifest(
    cas: &lillux::CasStore,
    digest: &str,
) -> anyhow::Result<Option<ExternalLargeContentManifestObject>> {
    let Some(value) = cas.get_object(digest)? else {
        return Ok(None);
    };
    if value.get("kind").and_then(serde_json::Value::as_str)
        != Some(EXTERNAL_LARGE_CONTENT_MANIFEST_KIND)
    {
        return Ok(None);
    }
    Ok(Some(ExternalLargeContentManifestObject::from_value(
        &value,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, size: u64) -> ExternalLargeContentManifestEntry {
        let chunk_size = MIN_LARGE_CONTENT_CHUNK_BYTES;
        let chunks = size.div_ceil(chunk_size);
        ExternalLargeContentManifestEntry {
            path: path.to_string(),
            kind: crate::objects::ExternalContentManifestEntryKind::File,
            mode: Some(0o644),
            blob_hash: None,
            file_sha256: Some("a".repeat(64)),
            size: Some(size),
            chunk_size: Some(chunk_size),
            chunk_hashes: (0..chunks).map(|_| "b".repeat(64)).collect(),
            target: None,
        }
    }

    fn manifest(
        entries: Vec<ExternalLargeContentManifestEntry>,
    ) -> ExternalLargeContentManifestObject {
        let total = entries.iter().filter_map(|entry| entry.size).sum();
        ExternalLargeContentManifestObject {
            schema: EXTERNAL_LARGE_CONTENT_SCHEMA.to_string(),
            kind: EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.to_string(),
            entry_count: entries.len(),
            entries,
            total_bytes: total,
        }
    }

    #[test]
    fn a_segmented_manifest_round_trips() {
        let mut entries = vec![
            entry("segment-00001.bin", 3 * MIN_LARGE_CONTENT_CHUNK_BYTES + 7),
            entry("segment-00002.bin", MIN_LARGE_CONTENT_CHUNK_BYTES),
        ];
        entries.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
        let object = manifest(entries);
        let value = object.to_value().unwrap();
        assert_eq!(
            ExternalLargeContentManifestObject::from_value(&value).unwrap(),
            object
        );
        assert!(!object.is_file_shaped());
        assert_eq!(object.referenced_large_objects(), vec!["a".repeat(64)]);
    }

    #[test]
    fn a_single_content_entry_is_file_shaped() {
        let object = manifest(vec![entry(crate::objects::FILE_REALIZATION_ENTRY_PATH, 1)]);
        object.validate().unwrap();
        assert!(object.is_file_shaped());
    }

    #[test]
    fn the_chunk_list_must_account_for_every_byte() {
        let mut object = manifest(vec![entry(
            "payload.bin",
            2 * MIN_LARGE_CONTENT_CHUNK_BYTES,
        )]);
        object.entries[0].chunk_hashes.pop();
        let error = object.validate().unwrap_err().to_string();
        assert!(error.contains("are required"), "got: {error}");
    }

    #[test]
    fn empty_content_and_oversized_claims_are_refused() {
        let object = manifest(vec![entry("payload.bin", 0)]);
        object.entries[0].chunk_hashes.is_empty();
        assert!(
            object
                .validate()
                .unwrap_err()
                .to_string()
                .contains("empty files belong in CAS")
        );

        let mut oversized = entry("payload.bin", MAX_LARGE_CONTENT_FILE_BYTES);
        oversized.size = Some(MAX_LARGE_CONTENT_FILE_BYTES + 1);
        oversized.chunk_hashes = (0..oversized
            .size
            .unwrap()
            .div_ceil(oversized.chunk_size.unwrap()))
            .map(|_| "b".repeat(64))
            .collect();
        // Chunk-list bytes for a >64 GiB claim blow the manifest byte bound
        // before the per-file bound answers; both refusals are fail-closed.
        assert!(manifest(vec![oversized]).validate().is_err());
    }

    #[test]
    fn chunk_size_is_bounded_to_the_current_schema_range() {
        let mut object = manifest(vec![entry("payload.bin", 8)]);
        object.entries[0].chunk_size = Some(512);
        object.entries[0].chunk_hashes = vec!["b".repeat(64)];
        let error = object.validate().unwrap_err().to_string();
        assert!(error.contains("powers of two"), "got: {error}");
    }

    #[test]
    fn entries_stay_strictly_ordered_by_path_bytes() {
        let object = manifest(vec![entry("b.bin", 1), entry("a.bin", 1)]);
        let error = object.validate().unwrap_err().to_string();
        assert!(error.contains("strictly ordered"), "got: {error}");
    }

    #[test]
    fn large_manifest_tree_requires_explicit_directory_ancestors() {
        let missing = manifest(vec![entry("models/weights.bin", 1)]);
        let error = missing.validate().unwrap_err().to_string();
        assert!(error.contains("absent directory ancestor"), "got: {error}");

        let mut ancestor = entry("models", 1);
        ancestor.file_sha256 = Some("c".repeat(64));
        let collision = manifest(vec![ancestor, entry("models/weights.bin", 1)]);
        let error = collision.validate().unwrap_err().to_string();
        assert!(error.contains("non-directory ancestor"), "got: {error}");

        let directory = ExternalLargeContentManifestEntry {
            path: "models".to_owned(),
            kind: crate::objects::ExternalContentManifestEntryKind::Dir,
            mode: None,
            blob_hash: None,
            file_sha256: None,
            size: None,
            chunk_size: None,
            chunk_hashes: Vec::new(),
            target: None,
        };
        let valid = manifest(vec![directory, entry("models/weights.bin", 1)]);
        valid.validate().unwrap();
    }

    #[test]
    fn a_large_tree_preserves_small_files_directories_and_symlinks() {
        let mut entries = vec![
            ExternalLargeContentManifestEntry {
                path: "bin".to_owned(),
                kind: crate::objects::ExternalContentManifestEntryKind::Dir,
                mode: None,
                blob_hash: None,
                file_sha256: None,
                size: None,
                chunk_size: None,
                chunk_hashes: Vec::new(),
                target: None,
            },
            ExternalLargeContentManifestEntry {
                path: "bin/compiler".to_owned(),
                kind: crate::objects::ExternalContentManifestEntryKind::File,
                mode: Some(0o755),
                blob_hash: Some("c".repeat(64)),
                file_sha256: None,
                size: Some(0),
                chunk_size: None,
                chunk_hashes: Vec::new(),
                target: None,
            },
            ExternalLargeContentManifestEntry {
                path: "bin/cc".to_owned(),
                kind: crate::objects::ExternalContentManifestEntryKind::Symlink,
                mode: None,
                blob_hash: None,
                file_sha256: None,
                size: None,
                chunk_size: None,
                chunk_hashes: Vec::new(),
                target: Some("compiler".to_owned()),
            },
        ];
        entries.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
        let object = manifest(entries);
        object.validate().unwrap();
        assert_eq!(object.referenced_blobs(), vec!["c".repeat(64)]);
        assert!(object.referenced_large_objects().is_empty());
    }
}
