//! Bundle manifest rebuild action.
//!
//! Walks `<bundle_root>/.ai/bin/<triple>/` for every triple, hashes
//! every non-sidecar file as a CAS blob, builds an `ItemSource` per
//! binary (storing the JSON object in CAS), then aggregates all entries
//! into a single `SourceManifest`. The manifest object is stored in CAS
//! and its hex hash is written to `<bundle_root>/.ai/refs/bundles/manifest`.
//!
//! The publish pipeline generates a separate `.ai/manifest.yaml` for
//! bundle-level metadata (name, version, provides_kinds).
//!
//! This is an explicit operator workflow. The daemon never invokes it.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::json;

use lillux::cas::CasStore;
pub use lillux::crypto::SigningKey;
use lillux::signature::{compute_fingerprint, sign_content};

/// Helper: load a signing key from a PEM file (delegates to lillux).
pub fn load_signing_key(path: &Path) -> Result<SigningKey> {
    lillux::crypto::load_signing_key(path)
}
use ryeos_state::objects::{ItemSource, SourceManifest};

#[derive(Debug, serde::Serialize)]
pub struct RebuiltEntry {
    pub item_ref: String,
    pub item_source_hash: String,
    pub blob_hash: String,
}

#[derive(Debug, serde::Serialize)]
pub struct RebuildReport {
    pub manifest_hash: String,
    pub entries: Vec<RebuiltEntry>,
    pub signer_fingerprint: String,
}

/// Recompute every bin/<triple>/* item source and the top-level
/// SourceManifest in CAS.
pub fn rebuild_bundle_manifest(
    bundle_root: &Path,
    signing_key: &SigningKey,
) -> Result<RebuildReport> {
    let bin_root = bundle_root.join(".ai/bin");
    if !bin_root.is_dir() {
        bail!(
            "no .ai/bin directory at {} — nothing to rebuild",
            bin_root.display()
        );
    }

    let cas_root = bundle_root.join(".ai/objects");
    fs::create_dir_all(&cas_root)
        .with_context(|| format!("create cas root {}", cas_root.display()))?;
    let cas = CasStore::new(cas_root);

    let fp = compute_fingerprint(&signing_key.verifying_key());

    let mut all_entries: HashMap<String, String> = HashMap::new();
    let mut report_entries: Vec<RebuiltEntry> = Vec::new();

    let mut triples: Vec<PathBuf> = fs::read_dir(&bin_root)
        .with_context(|| format!("read {}", bin_root.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.path())
        .collect();
    triples.sort();

    for triple_dir in &triples {
        let triple = triple_dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("non-utf8 triple dir name"))?
            .to_string();

        let mut binaries: Vec<PathBuf> = fs::read_dir(triple_dir)
            .with_context(|| format!("read {}", triple_dir.display()))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                if !p.is_file() {
                    return false;
                }
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.ends_with(".item_source.json") {
                    return false;
                }
                true
            })
            .collect();
        binaries.sort();

        for bin_path in &binaries {
            let bare = bin_path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| anyhow::anyhow!("non-utf8 binary name"))?
                .to_string();
            let item_ref = format!("bin/{triple}/{bare}");

            let bytes = fs::read(bin_path)
                .with_context(|| format!("read {}", bin_path.display()))?;
            let blob_hash = cas.store_blob(&bytes)?;

            let mode = unix_mode(bin_path)?;

            let item_source = ItemSource {
                item_ref: item_ref.clone(),
                content_blob_hash: blob_hash.clone(),
                integrity: format!("sha256:{blob_hash}"),
                signature_info: Some(json!({ "fingerprint": fp })),
                mode: Some(mode),
            };
            let item_source_value = item_source.to_value();
            let item_source_hash = cas.store_object(&item_source_value)?;

            // Sidecar (signed body) — canonical JSON of the ItemSource.
            let body = lillux::cas::canonical_json(&item_source_value);
            let signed_sidecar = sign_content(&body, signing_key, "#", None);
            let sidecar_path = bin_path.with_file_name(format!("{bare}.item_source.json"));
            atomic_write_str(&sidecar_path, &signed_sidecar)?;

            all_entries.insert(item_ref.clone(), item_source_hash.clone());
            report_entries.push(RebuiltEntry {
                item_ref,
                item_source_hash,
                blob_hash,
            });
        }
    }

    let source_manifest = SourceManifest {
        item_source_hashes: all_entries,
    };
    let manifest_value = source_manifest.to_value();
    let manifest_hash = cas.store_object(&manifest_value)?;

    // Refs file: hex hash + newline.
    let refs_dir = bundle_root.join(".ai/refs/bundles");
    fs::create_dir_all(&refs_dir)
        .with_context(|| format!("create {}", refs_dir.display()))?;
    let refs_path = refs_dir.join("manifest");
    atomic_write_str(&refs_path, &format!("{manifest_hash}\n"))?;

    Ok(RebuildReport {
        manifest_hash,
        entries: report_entries,
        signer_fingerprint: fp,
    })
}

fn atomic_write_str(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, content.as_bytes())
        .with_context(|| format!("write tmp {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn unix_mode(path: &Path) -> Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::metadata(path)
        .with_context(|| format!("metadata {}", path.display()))?;
    Ok(meta.permissions().mode() & 0o7777)
}


#[cfg(not(unix))]
fn unix_mode(_path: &Path) -> Result<u32> {
    Ok(0o755)
}
