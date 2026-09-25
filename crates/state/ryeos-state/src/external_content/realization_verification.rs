//! Exact descriptor-relative verification of a staged external realization.
//!
//! The caller supplies a retained manifest descriptor and content descriptor;
//! neither a path spelling nor a claimed hash alone authorizes worker input.

use std::collections::BTreeMap;
use std::ffi::OsStr;

use anyhow::{Context as _, Result, ensure};

use crate::objects::{
    EXTERNAL_CONTENT_MANIFEST_KIND, EXTERNAL_LARGE_CONTENT_MANIFEST_KIND, ExternalContentKind,
    ExternalContentManifestEntryKind, ExternalContentManifestObject,
    ExternalLargeContentManifestObject, MAX_LARGE_CONTENT_MANIFEST_BYTES,
};

#[derive(Clone)]
struct ExpectedEntry {
    kind: ExternalContentManifestEntryKind,
    size: Option<u64>,
    mode: Option<u32>,
    digest: Option<String>,
    chunk_size: Option<u64>,
    chunk_hashes: Vec<String>,
    target: Option<String>,
}

/// Verify a provider-staged realization against the exact manifest object
/// carried in a separate sealed product authority. The manifest kind is
/// explicit: a large object never falls back to the small capture tier.
pub fn verify_staged_external_realization(
    source: &lillux::InheritedDescriptorAuthority,
    manifest_source: &lillux::InheritedDescriptorAuthority,
    manifest_kind: &str,
    manifest_hash: &str,
    manifest_bytes: u64,
    content_kind: ExternalContentKind,
    expected_bytes: u64,
) -> Result<()> {
    let ceiling = match manifest_kind {
        EXTERNAL_CONTENT_MANIFEST_KIND => crate::objects::MAX_EXTERNAL_CONTENT_MANIFEST_BYTES,
        EXTERNAL_LARGE_CONTENT_MANIFEST_KIND => MAX_LARGE_CONTENT_MANIFEST_BYTES,
        _ => anyhow::bail!("staged realization has unsupported manifest kind"),
    };
    ensure!(
        manifest_bytes > 0 && manifest_bytes <= ceiling as u64,
        "staged realization manifest exceeds its selected tier"
    );
    let observation = manifest_source.regular_file_observation()?;
    ensure!(
        observation.size() == manifest_bytes
            && manifest_source.digest_regular_file_stable_exact(&observation)? == manifest_hash,
        "staged realization manifest descriptor changed"
    );
    let (bytes, after) = manifest_source.read_regular_file_stable_bounded(ceiling as u64)?;
    ensure!(
        after.size() == manifest_bytes
            && bytes.len() as u64 == manifest_bytes
            && lillux::sha256_hex(&bytes) == manifest_hash,
        "staged realization manifest bytes changed"
    );
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    ensure!(
        lillux::canonical_json(&value)?.as_bytes() == bytes,
        "staged realization manifest is not canonical"
    );
    let (entries, total_bytes, is_file_shaped) = match manifest_kind {
        EXTERNAL_CONTENT_MANIFEST_KIND => {
            let manifest = ExternalContentManifestObject::from_value(&value)?;
            let entries: BTreeMap<String, ExpectedEntry> = manifest
                .entries
                .iter()
                .map(|entry| {
                    (
                        entry.path.clone(),
                        ExpectedEntry {
                            kind: entry.kind,
                            size: entry.size,
                            mode: entry.mode,
                            digest: entry.blob_hash.clone(),
                            chunk_size: None,
                            chunk_hashes: Vec::new(),
                            target: entry.target.clone(),
                        },
                    )
                })
                .collect();
            (entries, manifest.total_bytes, manifest.is_file_shaped())
        }
        EXTERNAL_LARGE_CONTENT_MANIFEST_KIND => {
            let manifest = ExternalLargeContentManifestObject::from_value(&value)?;
            let entries: BTreeMap<String, ExpectedEntry> = manifest
                .entries
                .iter()
                .map(|entry| {
                    (
                        entry.path.clone(),
                        ExpectedEntry {
                            kind: entry.kind,
                            size: entry.size,
                            mode: entry.mode,
                            digest: entry
                                .blob_hash
                                .clone()
                                .or_else(|| entry.file_sha256.clone()),
                            chunk_size: entry.chunk_size,
                            chunk_hashes: entry.chunk_hashes.clone(),
                            target: entry.target.clone(),
                        },
                    )
                })
                .collect();
            (entries, manifest.total_bytes, manifest.is_file_shaped())
        }
        _ => unreachable!("manifest kind was closed above"),
    };
    ensure!(
        total_bytes == expected_bytes,
        "staged realization byte claim changed"
    );
    match content_kind {
        ExternalContentKind::File => {
            ensure!(
                is_file_shaped,
                "staged file realization has a tree manifest"
            );
            let entry = entries
                .get(crate::objects::FILE_REALIZATION_ENTRY_PATH)
                .context("staged file realization lost its fixed manifest entry")?;
            verify_file(source, entry)?;
        }
        ExternalContentKind::Tree => {
            let directory =
                source.try_clone_pinned_directory("<staged-external-realization>".into())?;
            let mut observed = Vec::with_capacity(entries.len());
            verify_directory(&directory, "", &entries, &mut observed)?;
            observed.sort();
            ensure!(
                observed
                    .iter()
                    .map(String::as_str)
                    .eq(entries.keys().map(String::as_str)),
                "staged realization has missing or extra entries"
            );
        }
    }
    Ok(())
}

fn verify_directory(
    directory: &lillux::PinnedDirectory,
    prefix: &str,
    expected: &BTreeMap<String, ExpectedEntry>,
    observed: &mut Vec<String>,
) -> Result<()> {
    for actual in directory.entries_no_follow_bounded(expected.len().saturating_add(1))? {
        let name = actual
            .name
            .to_str()
            .context("staged realization has a non-UTF-8 name")?;
        let path = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        let entry = expected
            .get(&path)
            .with_context(|| format!("staged realization contains undeclared entry {path}"))?;
        observed.push(path.clone());
        match entry.kind {
            ExternalContentManifestEntryKind::Dir => {
                ensure!(
                    actual.entry_type == lillux::PinnedEntryType::Directory,
                    "staged realization directory {path} changed kind"
                );
                let child = directory
                    .open_child_directory(OsStr::new(name))?
                    .with_context(|| format!("staged directory {path} disappeared"))?;
                verify_directory(&child, &path, expected, observed)?;
            }
            ExternalContentManifestEntryKind::File => {
                ensure!(
                    actual.entry_type == lillux::PinnedEntryType::Regular,
                    "staged realization file {path} changed kind"
                );
                let file = directory
                    .open_regular(OsStr::new(name), false)?
                    .with_context(|| format!("staged file {path} disappeared"))?;
                let size = entry.size.context("validated staged file lost its size")?;
                let (digest, chunks, metadata) = match entry.chunk_size {
                    Some(chunk_size) => lillux::digest_open_regular_file_stable_chunked_exact(
                        &file, size, chunk_size,
                    )?,
                    None => {
                        let (digest, metadata) =
                            lillux::digest_open_regular_file_stable_exact(&file, size)?;
                        (digest, Vec::new(), metadata)
                    }
                };
                ensure!(
                    entry.digest.as_deref() == Some(digest.as_str())
                        && entry.chunk_hashes == chunks
                        && entry.mode == Some(lillux::normalized_portable_regular_mode(&metadata)?),
                    "staged realization file {path} changed bytes or mode"
                );
            }
            ExternalContentManifestEntryKind::Symlink => {
                ensure!(
                    actual.entry_type == lillux::PinnedEntryType::Symlink,
                    "staged realization symlink {path} changed kind"
                );
                let target = directory
                    .read_symlink_target(
                        OsStr::new(name),
                        crate::objects::MAX_SYMLINK_TARGET_BYTES as usize,
                    )?
                    .with_context(|| format!("staged symlink {path} disappeared"))?;
                ensure!(
                    entry.target.as_deref().map(str::as_bytes) == Some(target.as_slice()),
                    "staged realization symlink {path} changed target"
                );
            }
        }
    }
    Ok(())
}

fn verify_file(source: &lillux::InheritedDescriptorAuthority, entry: &ExpectedEntry) -> Result<()> {
    let observation = source.regular_file_observation()?;
    let (digest, chunks) = match entry.chunk_size {
        Some(chunk_size) => {
            source.digest_regular_file_chunked_stable_exact(&observation, chunk_size)?
        }
        None => (
            source.digest_regular_file_stable_exact(&observation)?,
            Vec::new(),
        ),
    };
    ensure!(
        Some(observation.size()) == entry.size
            && Some(observation.portable_mode()?) == entry.mode
            && Some(digest.as_str()) == entry.digest.as_deref()
            && chunks == entry.chunk_hashes,
        "staged file realization changed bytes or mode"
    );
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn file_product_uses_manifest_identity_not_a_raw_file_hash() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("product");
        std::fs::write(&path, b"exact product bytes").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let directory = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let source = directory
            .open_inherited_regular(OsStr::new("product"), false)
            .unwrap()
            .unwrap();
        let manifest = ExternalContentManifestObject {
            schema: crate::objects::EXTERNAL_CONTENT_TREE_SCHEMA.into(),
            kind: EXTERNAL_CONTENT_MANIFEST_KIND.into(),
            entries: vec![crate::objects::ExternalContentManifestEntry {
                path: crate::objects::FILE_REALIZATION_ENTRY_PATH.into(),
                kind: ExternalContentManifestEntryKind::File,
                mode: Some(0o644),
                blob_hash: Some(lillux::sha256_hex(b"exact product bytes")),
                size: Some(19),
                target: None,
            }],
            entry_count: 1,
            total_bytes: 19,
        };
        let bytes = lillux::canonical_json(&serde_json::to_value(manifest).unwrap()).unwrap();
        let manifest_hash = lillux::sha256_hex(bytes.as_bytes());
        assert_ne!(manifest_hash, lillux::sha256_hex(b"exact product bytes"));
        let manifest_source =
            lillux::sealed_memfd(c"test-file-product-manifest", bytes.as_bytes()).unwrap();
        verify_staged_external_realization(
            &source,
            &manifest_source,
            EXTERNAL_CONTENT_MANIFEST_KIND,
            &manifest_hash,
            bytes.len() as u64,
            ExternalContentKind::File,
            19,
        )
        .unwrap();
        assert!(
            verify_staged_external_realization(
                &source,
                &manifest_source,
                EXTERNAL_CONTENT_MANIFEST_KIND,
                &lillux::sha256_hex(b"exact product bytes"),
                bytes.len() as u64,
                ExternalContentKind::File,
                19,
            )
            .is_err()
        );
    }

    #[test]
    fn large_manifest_verifies_whole_file_chunks_and_exact_tree() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("bin")).unwrap();
        let file_path = root.path().join("bin/codex");
        let mut output = std::fs::File::create(&file_path).unwrap();
        let block = vec![0x3d; 1024 * 1024];
        for _ in 0..33 {
            output.write_all(&block).unwrap();
        }
        drop(output);
        std::fs::set_permissions(&file_path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let size = 33 * 1024 * 1024;
        let (digest, chunks, _) = lillux::digest_open_regular_file_stable_chunked_exact(
            &std::fs::File::open(&file_path).unwrap(),
            size,
            1024 * 1024,
        )
        .unwrap();
        let entry = crate::objects::ExternalLargeContentManifestEntry {
            path: "bin/codex".into(),
            kind: ExternalContentManifestEntryKind::File,
            mode: Some(0o755),
            blob_hash: None,
            file_sha256: Some(digest),
            size: Some(size),
            chunk_size: Some(1024 * 1024),
            chunk_hashes: chunks,
            target: None,
        };
        let manifest = ExternalLargeContentManifestObject {
            schema: crate::objects::EXTERNAL_LARGE_CONTENT_SCHEMA.into(),
            kind: EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.into(),
            entries: vec![
                crate::objects::ExternalLargeContentManifestEntry {
                    path: "bin".into(),
                    kind: ExternalContentManifestEntryKind::Dir,
                    mode: None,
                    blob_hash: None,
                    file_sha256: None,
                    size: None,
                    chunk_size: None,
                    chunk_hashes: Vec::new(),
                    target: None,
                },
                entry,
            ],
            entry_count: 2,
            total_bytes: size,
        };
        let source = lillux::PinnedDirectory::open(root.path())
            .unwrap()
            .unwrap()
            .inherited_descriptor_authority()
            .unwrap();
        let verify = |manifest: &ExternalLargeContentManifestObject| {
            let bytes = lillux::canonical_json(&manifest.to_value().unwrap()).unwrap();
            let hash = lillux::sha256_hex(bytes.as_bytes());
            let manifest_source =
                lillux::sealed_memfd(c"test-large-manifest", bytes.as_bytes()).unwrap();
            verify_staged_external_realization(
                &source,
                &manifest_source,
                EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
                &hash,
                bytes.len() as u64,
                ExternalContentKind::Tree,
                size,
            )
        };
        verify(&manifest).unwrap();
        let mut wrong_chunk = manifest.clone();
        wrong_chunk.entries[1].chunk_hashes[0] = "f".repeat(64);
        assert!(verify(&wrong_chunk).is_err());
        std::fs::write(root.path().join("undeclared"), b"ambient").unwrap();
        assert!(verify(&manifest).is_err());
    }
}
