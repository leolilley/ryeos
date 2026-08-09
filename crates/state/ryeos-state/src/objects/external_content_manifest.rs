//! Content-addressed manifest for realized external content.
//!
//! The state layer treats a realization structurally, exactly as it treats a
//! restore manifest: it knows that entries name blobs and that those blobs
//! must stay reachable, and it knows nothing about what the content means.
//! Identity semantics — declaration, authority, ordering — belong to the
//! engine that built the manifest.
//!
//! Reachability is the reason this object is typed. A realization referenced
//! only from a sealed program's derived values would be invisible to closure
//! traversal, and garbage collection would reclaim blobs a resumable chain
//! still needs to execute against.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const EXTERNAL_CONTENT_MANIFEST_KIND: &str = "external_content_manifest";
pub const EXTERNAL_CONTENT_TREE_SCHEMA: &str = "ryeos.external_content.tree.v1";
pub const EXTERNAL_REALIZATIONS_DERIVED_KEY: &str = "effective_external_realizations";
/// The single manifest path a file-shaped realization stores its content
/// under. Wire-level: both manifest kinds spell file shape the same way.
pub const FILE_REALIZATION_ENTRY_PATH: &str = "content";
/// Matches the generic closure link ceiling: a manifest that cannot be
/// traversed is unusable however well it hashes.
pub const MAX_EXTERNAL_CONTENT_ENTRIES: usize = 10_000;
pub const MAX_EXTERNAL_CONTENT_MANIFEST_BYTES: usize = 1024 * 1024;
pub const MAX_EXTERNAL_CONTENT_FILE_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_EXTERNAL_CONTENT_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
/// Pure DoS sanity for byte claims on the tier-blind realization wire. The
/// real byte policy is per storage tier and lives where the manifest kind is
/// known: the content caps above on this object and in its capture walk, and
/// the large-content caps on that manifest object and at ingest.
pub const MAX_REALIZATION_CLAIMED_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
pub const MAX_EXTERNAL_CONTENT_PATH_BYTES: usize = 4096;
pub const MAX_INLINE_SYMLINK_TARGET_BYTES: usize = 1024;
pub const MAX_SYMLINK_TARGET_BYTES: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalContentKind {
    File,
    Tree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalContentMode {
    Pinned,
    Captured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalContentManifestEntryKind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentManifestEntry {
    pub path: String,
    pub kind: ExternalContentManifestEntryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_blob: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentManifestObject {
    pub schema: String,
    pub kind: String,
    pub entries: Vec<ExternalContentManifestEntry>,
    pub entry_count: usize,
    pub total_bytes: u64,
}

/// One identity-bearing external-content realization sealed in the composed
/// program. This durable wire type is shared by admission, recovery, and CAS
/// closure traversal; those boundaries must never infer it from an arbitrary
/// derived-value shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentRealization {
    pub id: String,
    pub kind: ExternalContentKind,
    pub mode: ExternalContentMode,
    pub manifest_hash: String,
    pub entry_count: usize,
    pub total_bytes: u64,
    pub mount: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExternalContentRealizationSet(Vec<ExternalContentRealization>);

impl ExternalContentRealizationSet {
    pub fn new(mut entries: Vec<ExternalContentRealization>) -> anyhow::Result<Self> {
        entries.sort_by(|left, right| left.id.cmp(&right.id));
        let set = Self(entries);
        set.validate()?;
        Ok(set)
    }

    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let set: Self = serde_json::from_value(value.clone())?;
        set.validate()?;
        Ok(set)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &ExternalContentRealization> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let mut previous_id: Option<&str> = None;
        let mut mounts = BTreeSet::new();
        let mut entry_total = 0usize;
        let mut byte_total = 0u64;
        for entry in &self.0 {
            validate_realization_id(&entry.id)?;
            super::validate_canonical_project_relative_path(&entry.mount)?;
            if entry.mount.len() > MAX_EXTERNAL_CONTENT_PATH_BYTES {
                anyhow::bail!(
                    "external realization mount exceeds {MAX_EXTERNAL_CONTENT_PATH_BYTES} bytes"
                );
            }
            super::thread_snapshot::validate_canonical_hash(
                "external realization manifest_hash",
                &entry.manifest_hash,
            )?;
            if entry.entry_count > MAX_EXTERNAL_CONTENT_ENTRIES {
                anyhow::bail!(
                    "external realization `{}` exceeds {MAX_EXTERNAL_CONTENT_ENTRIES} entries",
                    entry.id
                );
            }
            if entry.total_bytes > MAX_REALIZATION_CLAIMED_BYTES {
                anyhow::bail!(
                    "external realization `{}` claims more than {MAX_REALIZATION_CLAIMED_BYTES} bytes",
                    entry.id
                );
            }
            entry_total = entry_total
                .checked_add(entry.entry_count)
                .ok_or_else(|| anyhow::anyhow!("external realization entry count overflow"))?;
            byte_total = byte_total
                .checked_add(entry.total_bytes)
                .ok_or_else(|| anyhow::anyhow!("external realization byte count overflow"))?;
            if entry_total > MAX_EXTERNAL_CONTENT_ENTRIES
                || byte_total > MAX_REALIZATION_CLAIMED_BYTES
            {
                anyhow::bail!("external realization set exceeds the per-launch aggregate bound");
            }
            if let Some(previous) = previous_id
                && previous >= entry.id.as_str()
            {
                anyhow::bail!("external realization set is not strictly ordered by id");
            }
            previous_id = Some(&entry.id);
            if !mounts.insert(entry.mount.as_str()) {
                anyhow::bail!(
                    "external realization mount `{}` appears more than once",
                    entry.mount
                );
            }
        }

        let mounts = mounts.into_iter().collect::<Vec<_>>();
        for (index, left) in mounts.iter().enumerate() {
            for right in mounts.iter().skip(index + 1) {
                if path_contains(left, right) || path_contains(right, left) {
                    anyhow::bail!("external realization mounts `{left}` and `{right}` overlap");
                }
            }
        }
        Ok(())
    }

    pub fn manifest_hashes(&self) -> Vec<String> {
        self.0
            .iter()
            .map(|entry| entry.manifest_hash.clone())
            .collect()
    }
}

fn validate_realization_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty() || id.len() > 64 {
        anyhow::bail!("external realization id must be 1..=64 bytes");
    }
    if !id.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
    }) {
        anyhow::bail!("external realization id has an unsupported character: {id:?}");
    }
    Ok(())
}

fn path_contains(parent: &str, child: &str) -> bool {
    child
        .strip_prefix(parent)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

impl ExternalContentManifestObject {
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let manifest: Self = serde_json::from_value(value.clone())?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.kind != EXTERNAL_CONTENT_MANIFEST_KIND {
            anyhow::bail!("unexpected external content manifest kind: {}", self.kind);
        }
        if self.schema != EXTERNAL_CONTENT_TREE_SCHEMA {
            anyhow::bail!(
                "unexpected external content manifest schema: {}",
                self.schema
            );
        }
        if self.entry_count != self.entries.len() {
            anyhow::bail!("external content manifest entry count disagrees with its entries");
        }
        if self.entries.len() > MAX_EXTERNAL_CONTENT_ENTRIES {
            anyhow::bail!(
                "external content manifest carries {} entries; the bound is {MAX_EXTERNAL_CONTENT_ENTRIES}",
                self.entries.len()
            );
        }
        let canonical = lillux::canonical_json(&serde_json::to_value(self)?)?;
        if canonical.len() > MAX_EXTERNAL_CONTENT_MANIFEST_BYTES {
            anyhow::bail!(
                "external content manifest is {} bytes; the bound is {MAX_EXTERNAL_CONTENT_MANIFEST_BYTES}",
                canonical.len()
            );
        }
        if self.total_bytes > MAX_EXTERNAL_CONTENT_TOTAL_BYTES {
            anyhow::bail!(
                "external content manifest declares {} bytes; the bound is {MAX_EXTERNAL_CONTENT_TOTAL_BYTES}",
                self.total_bytes
            );
        }

        // `total_bytes` enters executable identity, so a manifest may not
        // assert a total its own entries do not sum to.
        let summed: u64 = self
            .entries
            .iter()
            .filter_map(|entry| entry.size)
            .try_fold(0u64, |total, size| total.checked_add(size))
            .ok_or_else(|| anyhow::anyhow!("external content manifest sizes overflow"))?;
        if summed != self.total_bytes {
            anyhow::bail!(
                "external content manifest declares {} total bytes but its entries sum to {summed}",
                self.total_bytes
            );
        }

        let mut previous: Option<&str> = None;
        for entry in &self.entries {
            super::validate_canonical_project_relative_path(&entry.path)?;
            if entry.path.len() > MAX_EXTERNAL_CONTENT_PATH_BYTES {
                anyhow::bail!(
                    "external content manifest path exceeds {MAX_EXTERNAL_CONTENT_PATH_BYTES} bytes"
                );
            }
            // Strict ordering is structural: a duplicate or out-of-order path
            // would make one manifest describe two different trees.
            if let Some(previous) = previous
                && previous.as_bytes() >= entry.path.as_bytes()
            {
                anyhow::bail!(
                    "external content manifest entries are not strictly ordered by path bytes"
                );
            }
            previous = Some(&entry.path);

            match entry.kind {
                ExternalContentManifestEntryKind::File => {
                    let hash = entry.blob_hash.as_deref().ok_or_else(|| {
                        anyhow::anyhow!("manifest file entry `{}` has no blob", entry.path)
                    })?;
                    crate::objects::thread_snapshot::validate_canonical_hash(
                        "external content blob_hash",
                        hash,
                    )?;
                    let size = entry.size.ok_or_else(|| {
                        anyhow::anyhow!("manifest file entry `{}` has no size", entry.path)
                    })?;
                    if size > MAX_EXTERNAL_CONTENT_FILE_BYTES {
                        anyhow::bail!(
                            "manifest file entry `{}` is {size} bytes; the bound is {MAX_EXTERNAL_CONTENT_FILE_BYTES}",
                            entry.path
                        );
                    }
                    if !matches!(entry.mode, Some(0o644) | Some(0o755)) {
                        anyhow::bail!(
                            "manifest file entry `{}` has an invalid normalized mode",
                            entry.path
                        );
                    }
                    if entry.target.is_some() || entry.target_blob.is_some() {
                        anyhow::bail!("manifest file entry `{}` carries a link target", entry.path);
                    }
                }
                ExternalContentManifestEntryKind::Dir => {
                    if entry.blob_hash.is_some()
                        || entry.size.is_some()
                        || entry.mode.is_some()
                        || entry.target.is_some()
                        || entry.target_blob.is_some()
                    {
                        anyhow::bail!("manifest directory entry `{}` carries content", entry.path);
                    }
                }
                ExternalContentManifestEntryKind::Symlink => {
                    if entry.blob_hash.is_some() || entry.size.is_some() || entry.mode.is_some() {
                        anyhow::bail!("manifest symlink entry `{}` carries a blob", entry.path);
                    }
                    match (entry.target.as_deref(), entry.target_blob.as_deref()) {
                        (Some(target), None) => {
                            if target.is_empty()
                                || target.as_bytes().contains(&0)
                                || target.len() > MAX_INLINE_SYMLINK_TARGET_BYTES
                            {
                                anyhow::bail!(
                                    "manifest symlink entry `{}` has an invalid inline target",
                                    entry.path
                                );
                            }
                        }
                        (None, Some(hash)) => {
                            crate::objects::thread_snapshot::validate_canonical_hash(
                                "external content target_blob",
                                hash,
                            )?;
                        }
                        (Some(_), Some(_)) => anyhow::bail!(
                            "manifest symlink entry `{}` carries both an inline and a stored target",
                            entry.path
                        ),
                        // A symlink without a target cannot be rebuilt, and a
                        // realization that cannot be rebuilt is not one.
                        (None, None) => anyhow::bail!(
                            "manifest symlink entry `{}` cannot be reconstructed without a target",
                            entry.path
                        ),
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether this manifest can back a file-shaped realization: exactly one
    /// regular-file entry at the shared fixed file path.
    pub fn is_file_shaped(&self) -> bool {
        self.entries.len() == 1
            && self.entries[0].path == FILE_REALIZATION_ENTRY_PATH
            && self.entries[0].kind == ExternalContentManifestEntryKind::File
    }

    /// Every blob this manifest needs in order to be materialized.
    pub fn referenced_blobs(&self) -> Vec<String> {
        let mut blobs = BTreeSet::new();
        for entry in &self.entries {
            if let Some(hash) = &entry.blob_hash {
                blobs.insert(hash.clone());
            }
            if let Some(hash) = &entry.target_blob {
                blobs.insert(hash.clone());
            }
        }
        blobs.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, kind: ExternalContentManifestEntryKind) -> ExternalContentManifestEntry {
        ExternalContentManifestEntry {
            path: path.to_string(),
            kind,
            mode: None,
            blob_hash: None,
            size: None,
            target: None,
            target_blob: None,
        }
    }

    fn manifest(entries: Vec<ExternalContentManifestEntry>) -> ExternalContentManifestObject {
        ExternalContentManifestObject {
            schema: EXTERNAL_CONTENT_TREE_SCHEMA.to_string(),
            kind: EXTERNAL_CONTENT_MANIFEST_KIND.to_string(),
            entry_count: entries.len(),
            entries,
            total_bytes: 0,
        }
    }

    #[test]
    fn a_symlink_without_a_target_cannot_be_admitted() {
        let object = manifest(vec![entry(
            "link",
            ExternalContentManifestEntryKind::Symlink,
        )]);
        let error = object.validate().unwrap_err().to_string();
        assert!(error.contains("cannot be reconstructed"), "got: {error}");
    }

    #[test]
    fn entries_must_be_strictly_ordered_by_path_bytes() {
        let mut first = entry("b", ExternalContentManifestEntryKind::Dir);
        first.path = "b".to_string();
        let object = manifest(vec![
            first,
            entry("a", ExternalContentManifestEntryKind::Dir),
        ]);
        let error = object.validate().unwrap_err().to_string();
        assert!(error.contains("strictly ordered"), "got: {error}");
    }

    #[test]
    fn referenced_blobs_cover_content_and_oversized_targets() {
        let mut file = entry("a", ExternalContentManifestEntryKind::File);
        file.blob_hash = Some("a".repeat(64));
        file.size = Some(1);
        file.mode = Some(0o644);
        let mut link = entry("b", ExternalContentManifestEntryKind::Symlink);
        link.target_blob = Some("b".repeat(64));

        let mut object = manifest(vec![file, link]);
        object.total_bytes = 1;
        object.validate().unwrap();
        assert_eq!(
            object.referenced_blobs(),
            vec!["a".repeat(64), "b".repeat(64)]
        );
    }

    fn realization(id: &str, mount: &str) -> ExternalContentRealization {
        ExternalContentRealization {
            id: id.to_string(),
            kind: ExternalContentKind::Tree,
            mode: ExternalContentMode::Captured,
            manifest_hash: "a".repeat(64),
            entry_count: 1,
            total_bytes: 10,
            mount: mount.to_string(),
        }
    }

    #[test]
    fn realization_set_round_trips_through_its_wire_value() {
        let set = ExternalContentRealizationSet::new(vec![
            realization("beta", "vendor/beta"),
            realization("alpha", "vendor/alpha"),
        ])
        .unwrap();
        let value = set.to_value().unwrap();
        let restored = ExternalContentRealizationSet::from_value(&value).unwrap();
        assert_eq!(restored, set);
        // Construction ordered by id, and the ordering survives the wire.
        assert_eq!(
            restored
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
    }

    #[test]
    fn overlapping_realization_mounts_are_rejected() {
        let error = ExternalContentRealizationSet::new(vec![
            realization("outer", "vendor/sim"),
            realization("inner", "vendor/sim/lib"),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("overlap"), "got {error}");
    }
}
