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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cas() -> (tempfile::TempDir, lillux::CasStore) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("cas");
        std::fs::create_dir_all(&root).unwrap();
        (dir, lillux::CasStore::new(root))
    }

    fn file_entry(
        path: &str,
        blob_hash: &str,
        size: u64,
    ) -> crate::objects::ExternalContentManifestEntry {
        crate::objects::ExternalContentManifestEntry {
            path: path.to_string(),
            kind: ExternalContentManifestEntryKind::File,
            mode: Some(0o644),
            blob_hash: Some(blob_hash.to_string()),
            size: Some(size),
            target: None,
            target_blob: None,
        }
    }

    fn manifest(
        entries: Vec<crate::objects::ExternalContentManifestEntry>,
        total_bytes: u64,
    ) -> ExternalContentManifestObject {
        ExternalContentManifestObject {
            schema: crate::objects::EXTERNAL_CONTENT_TREE_SCHEMA.to_string(),
            kind: crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND.to_string(),
            entry_count: entries.len(),
            entries,
            total_bytes,
        }
    }

    #[test]
    fn a_stored_manifest_closure_round_trips_from_cas() {
        let (_dir, cas) = temp_cas();
        let alpha = cas.store_blob(b"alpha bytes").unwrap();
        let omega = cas.store_blob(b"omega").unwrap();
        let stored = manifest(
            vec![
                file_entry("alpha.txt", &alpha, 11),
                file_entry("omega.txt", &omega, 5),
            ],
            16,
        );
        let hash = cas
            .store_object(&serde_json::to_value(&stored).unwrap())
            .unwrap();

        let closure = VerifiedExternalContentClosure::load(&cas, &hash).unwrap();
        assert_eq!(closure.manifest_hash(), hash);
        assert_eq!(closure.manifest(), &stored);
        assert_eq!(closure.verified_blob_sizes().get(&alpha), Some(&11));
        assert_eq!(closure.verified_blob_sizes().get(&omega), Some(&5));
    }

    #[test]
    fn closure_load_refuses_missing_and_lying_payloads() {
        let (_dir, cas) = temp_cas();

        // A manifest naming a blob CAS does not hold is unavailable, never
        // partially verified.
        let absent = manifest(vec![file_entry("ghost.txt", &"f".repeat(64), 3)], 3);
        let hash = cas
            .store_object(&serde_json::to_value(&absent).unwrap())
            .unwrap();
        let error = VerifiedExternalContentClosure::load(&cas, &hash).unwrap_err();
        assert!(format!("{error:#}").contains("ghost.txt"), "got {error:#}");

        // A manifest asserting a size its blob does not have is a lie about
        // executable identity, not a tolerable drift.
        let blob = cas.store_blob(b"12345").unwrap();
        let lying = manifest(vec![file_entry("short.txt", &blob, 6)], 6);
        let hash = cas
            .store_object(&serde_json::to_value(&lying).unwrap())
            .unwrap();
        let error = VerifiedExternalContentClosure::load(&cas, &hash).unwrap_err();
        assert!(
            format!("{error:#}").contains("expected 6"),
            "got {error:#}"
        );
    }
}

