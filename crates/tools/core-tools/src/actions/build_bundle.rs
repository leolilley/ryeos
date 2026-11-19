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

use anyhow::{Context, Result, bail};
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

/// Recompute every `bin/<triple>/*` item source and the top-level
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
    let has_binary_root = match fs::symlink_metadata(&bin_root) {
        Ok(_) => {
            require_real_directory(&bin_root, "bundle binary root")?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect bundle binary root {}", bin_root.display()));
        }
    };

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

    let triple_entries = if has_binary_root {
        sorted_dir_entries(&bin_root)?
    } else {
        Vec::new()
    };
    for triple_entry in triple_entries {
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
    let existing_ref = fs::read_to_string(&refs_path).ok();
    if !existing_ref.as_deref().is_some_and(|existing| {
        already_signed_manifest_ref(
            existing,
            ryeos_engine::executor_resolution::EXECUTOR_MANIFEST_REF_DOMAIN,
            &manifest_hash,
            signing_key,
        )
    }) {
        let signed_ref = sign_content(&ref_body, signing_key, "#", None);
        atomic_write_str(&refs_path, &signed_ref)?;
    }

    rebuild_picture_manifest(bundle_root, &cas, signing_key)?;

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

/// Cover every picture blob under `.ai/pictures` with a signed picture
/// manifest at `.ai/refs/bundles/pictures`.
///
/// The picture sources are checked first: every record carries its required
/// fields and an accepted licence, every blob has exactly one record, hashes to
/// it and matches its declared type, and the size caps hold. A failure here
/// fails the publish. Each blob becomes a non-executable `pictures/<sha256>`
/// item source; the blob itself stays under `.ai/pictures/blobs`. A bundle
/// with no pictures root carries no picture manifest ref.
fn rebuild_picture_manifest(
    bundle_root: &Path,
    cas: &CasStore,
    signing_key: &SigningKey,
) -> Result<()> {
    let ai_root = bundle_root.join(ryeos_engine::AI_DIR);
    let refs_path = ryeos_bundle::pictures::picture_manifest_ref_path(&ai_root);
    let Some(pictures) = ryeos_bundle::pictures::check_picture_sources(bundle_root)
        .context("picture sources failed the publish check")?
    else {
        // A stale ref must not outlive the pictures it covered.
        match fs::remove_file(&refs_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("remove stale {}", refs_path.display()));
            }
        }
        return Ok(());
    };

    let mut item_source_hashes: HashMap<String, String> = HashMap::new();
    for sha256 in pictures.entries.keys() {
        let item_ref = format!("{}{sha256}", ryeos_bundle::pictures::PICTURE_ITEM_REF_PREFIX);
        let item_source = ItemSource {
            item_ref: item_ref.clone(),
            content_blob_hash: sha256.clone(),
            integrity: format!("sha256:{sha256}"),
            signature_info: None,
            mode: Some(ryeos_engine::executor_resolution::PICTURE_ITEM_SOURCE_MODE),
        };
        let item_source_hash = cas.store_object(&item_source.to_value())?;
        item_source_hashes.insert(item_ref, item_source_hash);
    }
    let source_manifest = SourceManifest { item_source_hashes };
    let manifest_hash = cas.store_object(&source_manifest.to_value())?;

    let domain = ryeos_engine::executor_resolution::PICTURE_MANIFEST_REF_DOMAIN;
    let existing_ref = fs::read_to_string(&refs_path).ok();
    if !existing_ref.as_deref().is_some_and(|existing| {
        already_signed_manifest_ref(existing, domain, &manifest_hash, signing_key)
    }) {
        let ref_body = format!("{domain}\n{manifest_hash}\n");
        let signed_ref = sign_content(&ref_body, signing_key, "#", None);
        atomic_write_str(&refs_path, &signed_ref)?;
    }
    Ok(())
}

fn already_signed_manifest_ref(
    existing: &str,
    domain: &str,
    expected_manifest_hash: &str,
    signing_key: &SigningKey,
) -> bool {
    let verifying_key = signing_key.verifying_key();
    let fingerprint = compute_fingerprint(&verifying_key);
    ryeos_engine::executor_resolution::verify_signed_manifest_ref_in_domain(
        existing,
        domain,
        |candidate| (candidate == fingerprint).then_some(verifying_key),
        ryeos_engine::resolution::TrustClass::TrustedBundle,
    )
    .is_ok_and(|verified| {
        verified.manifest_hash == expected_manifest_hash
            && verified.signer_fingerprint == fingerprint
    })
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
    let parent_path = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("bundle executable has no parent"))?;
    let parent = lillux::PinnedDirectory::open(parent_path)?
        .ok_or_else(|| anyhow::anyhow!("bundle executable parent is unavailable"))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("bundle executable has no file name"))?;
    let file = parent.open_regular(name, false)?.ok_or_else(|| {
        anyhow::anyhow!(
            "bundle executable {} must be a regular file, not a symlink or special file",
            path.display()
        )
    })?;
    let mode = lillux::observe_open_regular_file(&file)?.full_permission_mode()?;
    if mode & !0o777 != 0 {
        bail!(
            "bundle executable {} has forbidden special permission bits ({mode:#o})",
            path.display()
        );
    }
    if mode & 0o022 != 0 {
        bail!(
            "bundle executable {} is group/other-writable ({mode:#o})",
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
        assert!(
            unix_mode(&binary)
                .unwrap_err()
                .to_string()
                .contains("not executable")
        );

        fs::set_permissions(&binary, fs::Permissions::from_mode(0o4755)).unwrap();
        assert!(
            unix_mode(&binary)
                .unwrap_err()
                .to_string()
                .contains("special permission bits")
        );

        fs::set_permissions(&binary, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(
            unix_mode(&binary)
                .unwrap_err()
                .to_string()
                .contains("group/other-writable")
        );

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
            assert!(format!("{error:#}").contains("symlink"));
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
            assert!(format!("{error:#}").contains("symlink"));
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

    #[test]
    fn declarative_bundle_publishes_a_signed_empty_executor_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        fs::create_dir_all(bundle.join(".ai")).unwrap();
        let key = test_signing_key();

        let report = rebuild_bundle_manifest(&bundle, &key).unwrap();
        assert!(report.entries.is_empty());
        assert!(bundle.join(".ai/objects").is_dir());
        let signed_ref = fs::read_to_string(bundle.join(".ai/refs/bundles/manifest")).unwrap();
        let verified = ryeos_engine::executor_resolution::verify_signed_executor_manifest_ref(
            &signed_ref,
            |candidate| {
                (candidate == compute_fingerprint(&key.verifying_key()))
                    .then_some(key.verifying_key())
            },
            ryeos_engine::resolution::TrustClass::TrustedBundle,
        )
        .unwrap();
        assert_eq!(verified.manifest_hash, report.manifest_hash);
    }
    /// A real 16x10 lossy WebP.
    const WEBP_16X10: [u8; 52] = [
        0x52, 0x49, 0x46, 0x46, 0x2c, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38,
        0x20, 0x20, 0x00, 0x00, 0x00, 0x70, 0x01, 0x00, 0x9d, 0x01, 0x2a, 0x10, 0x00, 0x0a, 0x00,
        0x01, 0x40, 0x26, 0x25, 0x94, 0x02, 0x74, 0x01, 0xa0, 0x00, 0x00, 0xf1, 0x3a, 0xbf, 0xbe,
        0xa7, 0x41, 0xf1, 0x5f, 0x00, 0x00, 0x00,
    ];

    fn picture_record(sha: &str, source_line: &str, licence: &str) -> String {
        format!(
            "title: Test\n{source_line}licence: \"{licence}\"\nrenditions:\n  - role: card\n    \
             sha256: \"{sha}\"\n    media_type: image/webp\n    width: 16\n    height: 10\n    \
             bytes: 52\n"
        )
    }

    /// A bundle with one picture, its record signed by `key`. Returns the
    /// bundle root and the blob hash.
    fn picture_bundle(
        tmp: &Path,
        key: &SigningKey,
        source_url: &str,
        licence: &str,
    ) -> (std::path::PathBuf, String) {
        let bundle = tmp.join("bundle");
        let pictures = bundle.join(".ai/pictures");
        fs::create_dir_all(pictures.join("blobs")).unwrap();
        fs::create_dir_all(pictures.join("ryeos")).unwrap();
        let sha = lillux::sha256_hex(&WEBP_16X10);
        fs::write(pictures.join(format!("blobs/{sha}.webp")), WEBP_16X10).unwrap();
        let source_line = if source_url.is_empty() {
            String::new()
        } else {
            format!("source_url: {source_url}\n")
        };
        let record = picture_record(&sha, &source_line, licence);
        fs::write(
            pictures.join("ryeos/test.yaml"),
            sign_content(&record, key, "#", None),
        )
        .unwrap();
        (bundle, sha)
    }

    fn trust_of(key: &SigningKey) -> ryeos_engine::trust::TrustStore {
        ryeos_engine::trust::TrustStore::from_signers(vec![ryeos_engine::trust::TrustedSigner {
            fingerprint: compute_fingerprint(&key.verifying_key()),
            verifying_key: key.verifying_key(),
            label: None,
        }])
    }

    #[test]
    fn pictures_are_covered_by_a_signed_non_executable_picture_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let key = test_signing_key();
        let (bundle, sha) =
            picture_bundle(tmp.path(), &key, "https://example.test/object/1", "cc0");

        let report = rebuild_bundle_manifest(&bundle, &key).unwrap();
        assert!(report.entries.is_empty(), "pictures are not executables");
        assert!(bundle.join(".ai/refs/bundles/pictures").is_file());

        let pictures =
            ryeos_bundle::pictures::verify_bundle_pictures(&bundle, &trust_of(&key), true)
                .unwrap();
        assert_eq!(pictures.entries.len(), 1);
        assert_eq!(pictures.entries[&sha].width, 16);

        // The executor manifest ref is a different signing domain: it does not
        // verify as a picture manifest.
        let executor_ref = fs::read_to_string(bundle.join(".ai/refs/bundles/manifest")).unwrap();
        fs::write(bundle.join(".ai/refs/bundles/pictures"), executor_ref).unwrap();
        assert!(
            ryeos_bundle::pictures::verify_bundle_pictures(&bundle, &trust_of(&key), false)
                .is_err()
        );
    }

    #[test]
    fn picture_rebuild_is_idempotent_and_drops_a_stale_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let key = test_signing_key();
        let (bundle, _) = picture_bundle(tmp.path(), &key, "https://example.test/object/1", "cc0");
        rebuild_bundle_manifest(&bundle, &key).unwrap();
        let first = fs::read_to_string(bundle.join(".ai/refs/bundles/pictures")).unwrap();
        rebuild_bundle_manifest(&bundle, &key).unwrap();
        let second = fs::read_to_string(bundle.join(".ai/refs/bundles/pictures")).unwrap();
        assert_eq!(first, second);

        fs::remove_dir_all(bundle.join(".ai/pictures")).unwrap();
        rebuild_bundle_manifest(&bundle, &key).unwrap();
        assert!(!bundle.join(".ai/refs/bundles/pictures").exists());
    }

    #[test]
    fn publish_refuses_a_picture_without_source_or_with_an_unaccepted_licence() {
        let key = test_signing_key();
        for (source_url, licence, needle) in [
            ("", "cc0", "source_url"),
            ("https://example.test/object/1", "cc-by", "licence"),
            ("https://example.test/object/1", "", "licence"),
            ("http://example.test/object/1", "cc0", "https"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let (bundle, _) = picture_bundle(tmp.path(), &key, source_url, licence);
            let error = rebuild_bundle_manifest(&bundle, &key)
                .expect_err("an incomplete picture record must fail the publish");
            let text = format!("{error:#}");
            assert!(text.contains(needle), "{text}");
            assert!(!bundle.join(".ai/refs/bundles/pictures").exists());
        }
    }

    #[test]
    fn publish_refuses_a_picture_whose_bytes_do_not_match_the_record() {
        let tmp = tempfile::tempdir().unwrap();
        let key = test_signing_key();
        let (bundle, sha) =
            picture_bundle(tmp.path(), &key, "https://example.test/object/1", "cc0");
        let mut tampered = WEBP_16X10.to_vec();
        tampered[30] ^= 1;
        fs::write(
            bundle.join(format!(".ai/pictures/blobs/{sha}.webp")),
            tampered,
        )
        .unwrap();
        let error = rebuild_bundle_manifest(&bundle, &key)
            .expect_err("a blob that no longer hashes to its record must fail the publish");
        assert!(format!("{error:#}").contains("hashes to"));
    }

    #[test]
    fn installed_picture_tampering_and_foreign_signers_do_not_verify() {
        let tmp = tempfile::tempdir().unwrap();
        let key = test_signing_key();
        let (bundle, sha) =
            picture_bundle(tmp.path(), &key, "https://example.test/object/1", "cc0");
        rebuild_bundle_manifest(&bundle, &key).unwrap();
        let blob = bundle.join(format!(".ai/pictures/blobs/{sha}.webp"));

        // Same length, one byte changed: only the deep check hashes it.
        let mut tampered = WEBP_16X10.to_vec();
        tampered[30] ^= 1;
        fs::write(&blob, tampered).unwrap();
        assert!(
            ryeos_bundle::pictures::verify_bundle_pictures(&bundle, &trust_of(&key), true)
                .is_err()
        );
        fs::write(&blob, WEBP_16X10).unwrap();

        // A node that does not trust the signer verifies nothing.
        let other = SigningKey::from_bytes(&[48; 32]);
        assert!(
            ryeos_bundle::pictures::verify_bundle_pictures(&bundle, &trust_of(&other), false)
                .is_err()
        );

        // A record edited after signing fails its signature.
        let record = bundle.join(".ai/pictures/ryeos/test.yaml");
        let edited = fs::read_to_string(&record)
            .unwrap()
            .replace("title: Test", "title: Changed");
        fs::write(&record, edited).unwrap();
        assert!(
            ryeos_bundle::pictures::verify_bundle_pictures(&bundle, &trust_of(&key), false)
                .is_err()
        );
    }
}
