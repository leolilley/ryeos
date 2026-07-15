//! Bundle manifest rebuild action.
//!
//! Walks `<bundle_root>/.ai/bin/<triple>/` for every triple, hashes
//! every non-sidecar file as a CAS blob, builds an `ItemSource` per
//! binary (storing the JSON object in CAS), then aggregates all entries
//! into a single `SourceManifest`. The manifest object is stored in CAS
//! and a signed ref containing its hex hash is written to
//! `<bundle_root>/.ai/refs/bundles/manifest`. The trusted ref signature is
//! the cryptographic root for manifest -> item-source -> blob resolution.
//!
//! The publish pipeline generates a separate `.ai/manifest.yaml` for
//! bundle-level metadata (name, version, provides_kinds).
//!
//! This is an explicit node-administrator workflow. The daemon never invokes it.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
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
    super::publisher_transaction::with_staged_bundle_generation(bundle_root, |staging| {
        rebuild_bundle_manifest_in_place(staging, signing_key)
    })
}

/// Rebuild publisher artifacts directly in `bundle_root`.
///
/// Callers must already own a complete staged bundle generation. The public
/// entry point above supplies that transaction; the full publish pipeline has
/// its own outer transaction and uses this helper to avoid nested copies.
pub(super) fn rebuild_bundle_manifest_in_place(
    bundle_root: &Path,
    signing_key: &SigningKey,
) -> Result<RebuildReport> {
    require_real_directory(bundle_root, "bundle root")?;
    let ai_root = bundle_root.join(".ai");
    validate_publish_control_tree(&ai_root, true)?;

    let bin_root = ai_root.join("bin");
    require_real_directory(&bin_root, "bundle binary root")?;

    // Validate every existing output component before creating or writing any
    // publisher output. This prevents CAS/ref publication through a linked
    // control root; the recursive check above also rejects linked CAS shards.
    let cas_root = ai_root.join("objects");
    let refs_root = ai_root.join("refs");
    let refs_dir = refs_root.join("bundles");
    require_optional_real_directory(&cas_root, "bundle CAS root")?;
    require_optional_real_directory(&refs_root, "bundle refs root")?;
    require_optional_real_directory(&refs_dir, "bundle refs/bundles root")?;
    ensure_real_directory(&cas_root, "bundle CAS root")?;
    ensure_real_directory(&refs_root, "bundle refs root")?;
    ensure_real_directory(&refs_dir, "bundle refs/bundles root")?;

    let cas = CasStore::new(cas_root);

    let fp = compute_fingerprint(&signing_key.verifying_key());

    let mut all_entries: HashMap<String, String> = HashMap::new();
    let mut report_entries: Vec<RebuiltEntry> = Vec::new();

    for triple_entry in sorted_dir_entries(&bin_root)? {
        let triple_type = triple_entry
            .file_type()
            .with_context(|| format!("inspect {}", triple_entry.path().display()))?;
        if triple_type.is_symlink() || !triple_type.is_dir() {
            bail!(
                "bundle binary root entry {} must be a regular target-triple directory",
                triple_entry.path().display()
            );
        }
        let triple_dir = triple_entry.path();
        let triple = triple_dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("non-utf8 triple dir name"))?
            .to_string();
        if !is_safe_executor_path_segment(&triple) {
            bail!("unsafe target-triple directory name `{triple}`");
        }

        let mut binaries = Vec::new();
        let mut sidecar_names = HashSet::new();
        for artifact in sorted_dir_entries(&triple_dir)? {
            let artifact_path = artifact.path();
            let artifact_type = artifact
                .file_type()
                .with_context(|| format!("inspect {}", artifact_path.display()))?;
            if artifact_type.is_symlink() || !artifact_type.is_file() {
                bail!(
                    "bundle binary artifact {} must be a regular file",
                    artifact_path.display()
                );
            }
            let artifact_name = artifact
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("non-utf8 binary artifact name"))?;
            if let Some(bin_name) = artifact_name.strip_suffix(".item_source.json") {
                if !is_safe_executor_path_segment(bin_name) {
                    bail!("unsafe ItemSource sidecar name `{artifact_name}`");
                }
                sidecar_names.insert(bin_name.to_string());
                continue;
            }
            if !is_safe_executor_path_segment(&artifact_name) {
                bail!("unsafe bundle executable name `{artifact_name}`");
            }
            binaries.push(artifact_path);
        }
        binaries.sort();
        let binary_names: HashSet<String> = binaries
            .iter()
            .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
            .map(str::to_string)
            .collect();
        if let Some(stale_sidecar) = sidecar_names.difference(&binary_names).next() {
            bail!(
                "stale ItemSource sidecar for missing executable `{stale_sidecar}` in {}",
                triple_dir.display()
            );
        }

        for bin_path in &binaries {
            let bare = bin_path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| anyhow::anyhow!("non-utf8 binary name"))?
                .to_string();
            let item_ref = format!("bin/{triple}/{bare}");

            let bytes =
                fs::read(bin_path).with_context(|| format!("read {}", bin_path.display()))?;
            let blob_hash = cas.store_blob(&bytes)?;

            let mode = unix_mode(bin_path)?;

            let item_source = ItemSource {
                item_ref: item_ref.clone(),
                content_blob_hash: blob_hash.clone(),
                integrity: format!("sha256:{blob_hash}"),
                // Trust is carried by the signed manifest ref. A fingerprint
                // claim inside this otherwise unsigned object would not be a
                // proof and is deliberately absent.
                signature_info: None,
                mode: Some(mode),
            };
            let item_source_value = item_source.to_value();
            let item_source_hash = cas.store_object(&item_source_value)?;

            // Sidecar (signed body) — canonical JSON of the ItemSource.
            let body = lillux::cas::canonical_json(&item_source_value)?;
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

    // Signed, domain-separated ref to the exact manifest object.
    let refs_path = refs_dir.join("manifest");
    let ref_body = format!(
        "{}\n{manifest_hash}\n",
        ryeos_engine::executor_resolution::EXECUTOR_MANIFEST_REF_DOMAIN
    );
    let signed_ref = sign_content(&ref_body, signing_key, "#", None);
    atomic_write_str(&refs_path, &signed_ref)?;

    Ok(RebuildReport {
        manifest_hash,
        entries: report_entries,
        signer_fingerprint: fp,
    })
}

fn atomic_write_str(path: &Path, content: &str) -> Result<()> {
    lillux::atomic_write(path, content.as_bytes())
        .with_context(|| format!("atomically write {}", path.display()))
}

fn require_real_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        bail!("{label} {} must be a real directory", path.display());
    }
    Ok(())
}

fn require_optional_real_directory(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.file_type().is_dir() => {
            Ok(())
        }
        Ok(_) => bail!("{label} {} must be a real directory", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {label} {}", path.display())),
    }
}

fn ensure_real_directory(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => require_real_directory(path, label),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).with_context(|| format!("create {label} {}", path.display()))?;
            require_real_directory(path, label)
        }
        Err(error) => Err(error).with_context(|| format!("inspect {label} {}", path.display())),
    }
}

/// Reject symlinks and special entries anywhere under `.ai` before the
/// publisher reads executable inputs or writes authorization material.
fn validate_publish_control_tree(path: &Path, tree_root: bool) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect publisher control-tree path {}", path.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        bail!(
            "publisher control tree contains symlink at {}",
            path.display()
        );
    }
    if file_type.is_dir() {
        for entry in sorted_dir_entries(path)? {
            validate_publish_control_tree(&entry.path(), false)?;
        }
        return Ok(());
    }
    if !tree_root && file_type.is_file() {
        return Ok(());
    }
    if tree_root {
        bail!(
            "publisher control tree root {} must be a real directory",
            path.display()
        );
    }
    bail!(
        "publisher control tree contains non-regular entry at {}",
        path.display()
    )
}

fn sorted_dir_entries(path: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("read {}", path.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("read {}", path.display()))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn is_safe_executor_path_segment(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('.')
        && !value.contains("..")
        && !value.contains('/')
        && !value.contains('\\')
        && !value.chars().any(char::is_whitespace)
        && !value.chars().any(char::is_control)
}

#[cfg(unix)]
fn unix_mode(path: &Path) -> Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    let meta =
        fs::symlink_metadata(path).with_context(|| format!("metadata {}", path.display()))?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        bail!(
            "bundle executable {} must be a regular file, not a symlink or special file",
            path.display()
        );
    }
    let mode = meta.permissions().mode() & 0o7777;
    if mode & !0o777 != 0 {
        bail!(
            "bundle executable {} has forbidden special permission bits ({mode:#o})",
            path.display()
        );
    }
    if mode & 0o111 == 0 {
        bail!(
            "bundle executable {} is not executable ({mode:#o})",
            path.display()
        );
    }
    Ok(mode)
}

#[cfg(not(unix))]
fn unix_mode(_path: &Path) -> Result<u32> {
    Ok(0o755)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn test_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[47; 32])
    }

    #[test]
    fn publisher_rejects_non_executable_and_special_modes() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = tmp.path().join("executor");
        fs::write(&binary, b"binary").unwrap();

        fs::set_permissions(&binary, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(unix_mode(&binary)
            .unwrap_err()
            .to_string()
            .contains("not executable"));

        fs::set_permissions(&binary, fs::Permissions::from_mode(0o4755)).unwrap();
        assert!(unix_mode(&binary)
            .unwrap_err()
            .to_string()
            .contains("special permission bits"));

        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(unix_mode(&binary).unwrap(), 0o755);
    }

    #[test]
    fn publisher_rejects_linked_ai_and_binary_roots_before_writing() {
        use std::os::unix::fs::symlink;

        for linked_root in [".ai", ".ai/bin"] {
            let tmp = tempfile::tempdir().unwrap();
            let bundle = tmp.path().join("bundle");
            let external = tmp.path().join("external");
            fs::create_dir_all(&bundle).unwrap();
            fs::create_dir_all(&external).unwrap();
            if linked_root == ".ai" {
                symlink(&external, bundle.join(".ai")).unwrap();
            } else {
                fs::create_dir(bundle.join(".ai")).unwrap();
                symlink(&external, bundle.join(".ai/bin")).unwrap();
            }

            let error = rebuild_bundle_manifest(&bundle, &test_signing_key())
                .expect_err("linked publisher input roots must be refused");
            assert!(error.to_string().contains("symlink"));
            assert!(!bundle.join(".ai/objects").exists());
        }
    }

    #[test]
    fn publisher_rejects_linked_cas_and_ref_roots_before_writing() {
        use std::os::unix::fs::symlink;

        for linked_root in ["objects", "refs"] {
            let tmp = tempfile::tempdir().unwrap();
            let bundle = tmp.path().join("bundle");
            let ai = bundle.join(".ai");
            let external = tmp.path().join("external");
            fs::create_dir_all(ai.join("bin")).unwrap();
            fs::create_dir(&external).unwrap();
            symlink(&external, ai.join(linked_root)).unwrap();

            let error = rebuild_bundle_manifest(&bundle, &test_signing_key())
                .expect_err("linked publisher output roots must be refused");
            assert!(error.to_string().contains("symlink"));
            assert!(fs::read_dir(&external).unwrap().next().is_none());
        }
    }

    #[test]
    fn publisher_rejects_non_directory_control_root_before_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let ai = bundle.join(".ai");
        fs::create_dir_all(ai.join("bin")).unwrap();
        fs::write(ai.join("objects"), b"not a directory").unwrap();

        let error = rebuild_bundle_manifest(&bundle, &test_signing_key())
            .expect_err("non-directory publisher output roots must be refused");
        assert!(error.to_string().contains("must be a real directory"));
        assert!(!ai.join("refs").exists());
    }
}
