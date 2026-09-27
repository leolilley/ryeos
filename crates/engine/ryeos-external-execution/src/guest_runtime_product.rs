//! Produce one exact, credential-free guest owner runtime tree.
//!
//! The caller supplies an admitted executable descriptor and controller
//! *public* root. This module does not publish a Render snapshot or qualify
//! the provider's restored bytes, process lifetime, or filesystem custody.

use std::ffi::OsStr;
use std::io::Cursor;

use anyhow::{Context as _, Result, ensure};
use base64::Engine as _;
use lillux::crypto::VerifyingKey;
use ryeos_state::objects::{
    EXTERNAL_CONTENT_MANIFEST_KIND, EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
    ExternalContentManifestEntryKind, ExternalContentManifestObject,
    ExternalLargeContentManifestObject,
};
use serde_json::Value;

use crate::guest_import_authorization::{GuestOwnerRuntimeProfile, ObservedGuestRuntime};

const OWNER_NAME: &str = "ryeos-external-guest-occurrence-owner";
const MAX_OWNER_EXECUTABLE_BYTES: u64 = 256 * 1024 * 1024;

/// Source-derived expectations for an independently observed guest snapshot.
/// The caller must first authenticate the product witness and open its exact
/// manifest through the retained CAS authority; this parser grants no provider
/// or allocation authority by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestOwnerRuntimeManifestIdentity {
    pub manifest_hash: String,
    pub owner_executable_sha256: String,
    pub controller_root_blob_sha256: String,
    pub controller_public_root: String,
}

struct EntryView {
    path: String,
    kind: ExternalContentManifestEntryKind,
    mode: Option<u32>,
    sha256: Option<String>,
    size: Option<u64>,
}

pub fn derive_guest_owner_runtime_manifest_identity(
    value: &Value,
    controller_root: &VerifyingKey,
) -> Result<GuestOwnerRuntimeManifestIdentity> {
    ensure!(
        !controller_root.is_weak(),
        "guest runtime controller root is weak"
    );
    let manifest_hash = ryeos_state::objects::canonical_value_digest(value)?;
    let entries = match value.get("kind").and_then(Value::as_str) {
        Some(EXTERNAL_CONTENT_MANIFEST_KIND) => {
            let manifest = ExternalContentManifestObject::from_value(value)?;
            manifest
                .entries
                .iter()
                .map(|entry| EntryView {
                    path: entry.path.clone(),
                    kind: entry.kind,
                    mode: entry.mode,
                    sha256: entry.blob_hash.clone(),
                    size: entry.size,
                })
                .collect::<Vec<_>>()
        }
        Some(EXTERNAL_LARGE_CONTENT_MANIFEST_KIND) => {
            let manifest = ExternalLargeContentManifestObject::from_value(value)?;
            manifest
                .entries
                .iter()
                .map(|entry| EntryView {
                    path: entry.path.clone(),
                    kind: entry.kind,
                    mode: entry.mode,
                    sha256: entry
                        .file_sha256
                        .clone()
                        .or_else(|| entry.blob_hash.clone()),
                    size: entry.size,
                })
                .collect::<Vec<_>>()
        }
        _ => anyhow::bail!("guest runtime product has an unsupported manifest kind"),
    };
    let owner = entries
        .iter()
        .find(|entry| entry.path == "bin/ryeos-external-guest-occurrence-owner")
        .context("guest runtime product has no exact owner executable")?;
    ensure!(
        owner.kind == ExternalContentManifestEntryKind::File
            && owner.mode == Some(0o755)
            && owner
                .size
                .is_some_and(|size| (1..=MAX_OWNER_EXECUTABLE_BYTES).contains(&size)),
        "guest runtime owner executable has the wrong manifest shape"
    );
    let owner_executable_sha256 = owner
        .sha256
        .as_deref()
        .context("guest runtime owner executable has no content identity")?;
    ensure!(
        lillux::valid_hash(owner_executable_sha256),
        "guest runtime owner executable digest is invalid"
    );
    let root = entries
        .iter()
        .find(|entry| entry.path == "controller-root.hex")
        .context("guest runtime product has no controller-root file")?;
    let expected_root = lillux::sha256_hex(hex::encode(controller_root.to_bytes()).as_bytes());
    ensure!(
        root.kind == ExternalContentManifestEntryKind::File
            && root.mode == Some(0o644)
            && root.size == Some(64)
            && root.sha256.as_deref() == Some(expected_root.as_str()),
        "guest runtime product does not contain the expected controller root"
    );
    let profile = entries
        .iter()
        .find(|entry| entry.path == "guest-owner-profile.json")
        .context("guest runtime product has no owner profile")?;
    ensure!(
        profile.kind == ExternalContentManifestEntryKind::File
            && profile.mode == Some(0o644)
            && profile
                .size
                .is_some_and(|size| (1..=4 * 1024).contains(&size))
            && profile.sha256.as_deref().is_some_and(lillux::valid_hash),
        "guest runtime product has an invalid owner profile"
    );
    Ok(GuestOwnerRuntimeManifestIdentity {
        manifest_hash,
        owner_executable_sha256: owner_executable_sha256.to_owned(),
        controller_root_blob_sha256: expected_root,
        controller_public_root: format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(controller_root.to_bytes())
        ),
    })
}

pub struct GuestOwnerRuntimeProduct {
    root: lillux::PinnedDirectory,
    manifest_hash: String,
}

impl GuestOwnerRuntimeProduct {
    pub fn root(&self) -> &lillux::PinnedDirectory {
        &self.root
    }

    pub fn manifest_hash(&self) -> &str {
        &self.manifest_hash
    }
}

/// Create a fresh runtime child under an admitted private parent. The exact
/// resulting content-manifest digest is computed from the same tree the guest observes.
/// A failure leaves a non-publishable child for the caller to quarantine;
/// this function never overwrites an existing product.
pub fn produce_guest_owner_runtime(
    parent: &lillux::PinnedDirectory,
    name: &OsStr,
    owner_executable: &lillux::InheritedDescriptorAuthority,
    owner_bytes: u64,
    owner_sha256: &str,
    controller_root: &VerifyingKey,
    profile: &GuestOwnerRuntimeProfile,
) -> Result<GuestOwnerRuntimeProduct> {
    parent.require_owner_private_directory()?;
    profile.validate()?;
    ensure!(
        !controller_root.is_weak(),
        "guest runtime controller root is weak"
    );
    ensure!(
        (1..=MAX_OWNER_EXECUTABLE_BYTES).contains(&owner_bytes) && lillux::valid_hash(owner_sha256),
        "guest runtime owner executable exceeds its exact bound"
    );
    owner_executable.require_owned_executable()?;
    let observation = owner_executable.regular_file_observation()?;
    ensure!(
        observation.size() == owner_bytes,
        "guest runtime owner executable changed length"
    );
    ensure!(
        owner_executable.digest_regular_file_stable_exact(&observation)? == owner_sha256,
        "guest runtime owner executable changed before production"
    );

    let root = parent.create_child(name, 0o700)?;
    root.require_owner_private_directory()?;
    let bin = root.create_child(OsStr::new("bin"), 0o700)?;
    let mut source = owner_executable.stable_regular_reader_exact(
        owner_bytes,
        owner_sha256,
        MAX_OWNER_EXECUTABLE_BYTES,
    )?;
    let (written, copied) = bin
        .atomic_create_regular_from_reader(
            OsStr::new(OWNER_NAME),
            &mut source,
            MAX_OWNER_EXECUTABLE_BYTES,
            0o555,
        )?
        .context("guest runtime owner executable already exists")?;
    source.finish()?;
    ensure!(
        copied == owner_bytes
            && lillux::digest_open_regular_file_stable_exact(&written, copied)?.0 == owner_sha256,
        "guest runtime owner executable changed during production"
    );

    let root_hex = hex::encode(controller_root.to_bytes());
    root.atomic_create_regular_from_reader(
        OsStr::new("controller-root.hex"),
        &mut Cursor::new(root_hex.as_bytes()),
        64,
        0o444,
    )?
    .context("guest runtime controller root already exists")?;
    let profile_bytes = ryeos_external_execution_contract::canonical_json(profile)?;
    root.atomic_create_regular_from_reader(
        OsStr::new("guest-owner-profile.json"),
        &mut Cursor::new(&profile_bytes),
        4 * 1024,
        0o444,
    )?
    .context("guest runtime profile already exists")?;
    let observed = ObservedGuestRuntime::observe(&root)?;
    Ok(GuestOwnerRuntimeProduct {
        root,
        manifest_hash: observed.manifest_hash().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::SigningKey;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn produces_exact_runtime_once_from_admitted_owner() {
        let source = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let executable = source.path().join("owner");
        let bytes = b"exact-guest-owner-fixture";
        std::fs::write(&executable, bytes).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let source = lillux::PinnedDirectory::open(source.path())
            .unwrap()
            .unwrap();
        let file = source
            .open_pinned_regular(OsStr::new("owner"), false)
            .unwrap()
            .unwrap();
        let authority = file.inherited_descriptor_authority().unwrap();
        let parent = lillux::PinnedDirectory::open(parent.path())
            .unwrap()
            .unwrap();
        let key = SigningKey::from_bytes(&[43; 32]).verifying_key();
        let profile = GuestOwnerRuntimeProfile {
            schema: 1,
            private_source_max_bytes: 32 * 1024 * 1024,
            private_source_max_inodes: 1024,
            owner_timeout_seconds: 600,
        };
        let digest = lillux::sha256_hex(bytes);
        let mut invalid_profile = profile.clone();
        invalid_profile.owner_timeout_seconds = 0;
        assert!(
            produce_guest_owner_runtime(
                &parent,
                OsStr::new("invalid-profile"),
                &authority,
                bytes.len() as u64,
                &digest,
                &key,
                &invalid_profile,
            )
            .is_err()
        );
        assert!(
            parent
                .open_child_directory(OsStr::new("invalid-profile"))
                .unwrap()
                .is_none()
        );
        assert!(
            produce_guest_owner_runtime(
                &parent,
                OsStr::new("wrong"),
                &authority,
                bytes.len() as u64,
                &"0".repeat(64),
                &key,
                &profile,
            )
            .is_err()
        );
        assert!(
            parent
                .open_child_directory(OsStr::new("wrong"))
                .unwrap()
                .is_none()
        );
        let product = produce_guest_owner_runtime(
            &parent,
            OsStr::new("runtime"),
            &authority,
            bytes.len() as u64,
            &digest,
            &key,
            &profile,
        )
        .unwrap();
        assert_eq!(
            product.manifest_hash(),
            ObservedGuestRuntime::observe(product.root())
                .unwrap()
                .manifest_hash()
        );
        let manifest = ryeos_state::observe_external_content_tree_exact(product.root()).unwrap();
        let manifest_value = serde_json::to_value(&manifest).unwrap();
        let identity = derive_guest_owner_runtime_manifest_identity(&manifest_value, &key).unwrap();
        assert_eq!(identity.manifest_hash, product.manifest_hash());
        assert_eq!(identity.owner_executable_sha256, digest);
        assert!(identity.controller_public_root.starts_with("ed25519:"));
        assert!(
            derive_guest_owner_runtime_manifest_identity(
                &manifest_value,
                &SigningKey::from_bytes(&[44; 32]).verifying_key(),
            )
            .is_err()
        );
        let mut tampered = manifest_value.clone();
        let owner_entry = tampered["entries"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|entry| entry["path"] == "bin/ryeos-external-guest-occurrence-owner")
            .unwrap();
        owner_entry["blob_hash"] = serde_json::json!("bad");
        assert!(derive_guest_owner_runtime_manifest_identity(&tampered, &key).is_err());
        let mut large = manifest_value.clone();
        large["kind"] = serde_json::json!(EXTERNAL_LARGE_CONTENT_MANIFEST_KIND);
        large["schema"] = serde_json::json!(ryeos_state::objects::EXTERNAL_LARGE_CONTENT_SCHEMA);
        let large_identity = derive_guest_owner_runtime_manifest_identity(&large, &key).unwrap();
        assert_eq!(large_identity.owner_executable_sha256, digest);
        let large_owner = large["entries"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|entry| entry["path"] == "bin/ryeos-external-guest-occurrence-owner")
            .unwrap();
        large_owner.as_object_mut().unwrap().remove("blob_hash");
        large_owner["file_sha256"] = serde_json::json!(digest);
        large_owner["chunk_size"] = serde_json::json!(1024 * 1024);
        large_owner["chunk_hashes"] = serde_json::json!([digest]);
        let large_object_identity =
            derive_guest_owner_runtime_manifest_identity(&large, &key).unwrap();
        assert_eq!(large_object_identity.owner_executable_sha256, digest);
        let different_root = SigningKey::from_bytes(&[44; 32]).verifying_key();
        let changed_controller = produce_guest_owner_runtime(
            &parent,
            OsStr::new("changed-controller"),
            &authority,
            bytes.len() as u64,
            &digest,
            &different_root,
            &profile,
        )
        .unwrap();
        assert_ne!(
            product.manifest_hash(),
            changed_controller.manifest_hash(),
            "a different controller trust root must change the runtime identity"
        );
        let mut changed_profile = profile.clone();
        changed_profile.owner_timeout_seconds -= 1;
        let changed_policy = produce_guest_owner_runtime(
            &parent,
            OsStr::new("changed-profile"),
            &authority,
            bytes.len() as u64,
            &digest,
            &key,
            &changed_profile,
        )
        .unwrap();
        assert_ne!(
            product.manifest_hash(),
            changed_policy.manifest_hash(),
            "a different owner profile must change the runtime identity"
        );
        assert_eq!(
            std::fs::read(parent.path().join("runtime/bin").join(OWNER_NAME)).unwrap(),
            bytes
        );
        let installed_owner = parent.path().join("runtime/bin").join(OWNER_NAME);
        std::fs::set_permissions(&installed_owner, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(&installed_owner, b"changed-guest-owner-fixture").unwrap();
        std::fs::set_permissions(&installed_owner, std::fs::Permissions::from_mode(0o555)).unwrap();
        assert_ne!(
            product.manifest_hash(),
            ObservedGuestRuntime::observe(product.root())
                .unwrap()
                .manifest_hash()
        );
        assert!(
            produce_guest_owner_runtime(
                &parent,
                OsStr::new("runtime"),
                &authority,
                bytes.len() as u64,
                &digest,
                &key,
                &profile,
            )
            .is_err()
        );
    }
}
