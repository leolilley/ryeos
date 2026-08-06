//! Verified CAS closure for one realized external-content manifest.

use std::collections::BTreeMap;

use anyhow::Context as _;

use crate::objects::{
    ExternalContentManifestEntryKind, ExternalContentManifestObject,
    MAX_EXTERNAL_CONTENT_FILE_BYTES, MAX_EXTERNAL_CONTENT_MANIFEST_BYTES,
    MAX_SYMLINK_TARGET_BYTES,
};

/// Manifest plus every verified payload required to materialize it.
///
/// Construction is the authority boundary: callers cannot obtain this value
/// from hash-shaped metadata or blob presence alone. Every object/blob is
/// loaded through its exact content address and checked against the manifest's
/// semantic bounds.
#[derive(Debug, Clone)]
pub struct VerifiedExternalContentClosure {
    manifest_hash: String,
    manifest: ExternalContentManifestObject,
    verified_blob_sizes: BTreeMap<String, u64>,
}

impl VerifiedExternalContentClosure {
    pub fn load(cas: &lillux::CasStore, manifest_hash: &str) -> anyhow::Result<Self> {
        let value = crate::object_closure::load_exact_cas_object_with_cas(
            cas,
            manifest_hash,
            MAX_EXTERNAL_CONTENT_MANIFEST_BYTES as u64,
        )
        .with_context(|| format!("load external content manifest {manifest_hash}"))?;
        let manifest = ExternalContentManifestObject::from_value(&value)?;
        let mut verified_blob_sizes = BTreeMap::new();

        for entry in &manifest.entries {
            let (hash, max_bytes, expected_size) = match entry.kind {
                ExternalContentManifestEntryKind::File => (
                    entry
                        .blob_hash
                        .as_deref()
                        .expect("validated file entry has blob hash"),
                    MAX_EXTERNAL_CONTENT_FILE_BYTES,
                    entry.size,
                ),
                ExternalContentManifestEntryKind::Symlink => {
                    let Some(hash) = entry.target_blob.as_deref() else {
                        continue;
                    };
                    (hash, MAX_SYMLINK_TARGET_BYTES, None)
                }
                ExternalContentManifestEntryKind::Dir => continue,
            };
            let bytes = crate::object_closure::load_exact_cas_blob_with_cas(
                cas, hash, max_bytes,
            )
            .with_context(|| {
                format!(
                    "load external content blob {hash} for manifest entry {}",
                    entry.path
                )
            })?;
            let size = u64::try_from(bytes.len())
                .map_err(|_| anyhow::anyhow!("external content blob size overflow"))?;
            if let Some(expected_size) = expected_size
                && size != expected_size
            {
                anyhow::bail!(
                    "external content blob {hash} for {} has size {size}, expected {expected_size}",
                    entry.path
                );
            }
            if entry.kind == ExternalContentManifestEntryKind::Symlink
                && (bytes.is_empty() || bytes.contains(&0))
            {
                anyhow::bail!(
                    "external content symlink target blob {hash} for {} is invalid",
                    entry.path
                );
            }
            verified_blob_sizes.insert(hash.to_string(), size);
        }

        Ok(Self {
            manifest_hash: manifest_hash.to_string(),
            manifest,
            verified_blob_sizes,
        })
    }

    pub fn manifest_hash(&self) -> &str {
        &self.manifest_hash
    }

    pub fn manifest(&self) -> &ExternalContentManifestObject {
        &self.manifest
    }

    pub fn verified_blob_sizes(&self) -> &BTreeMap<String, u64> {
        &self.verified_blob_sizes
    }
}

