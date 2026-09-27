//! Produce one exact, credential-free guest owner runtime tree.
//!
//! The caller supplies an admitted executable descriptor and controller
//! *public* root. This module does not publish a Render snapshot or qualify
//! the provider's restored bytes, process lifetime, or filesystem custody.

use std::ffi::OsStr;
use std::io::Cursor;

use anyhow::{Context as _, Result, ensure};
use lillux::crypto::VerifyingKey;

use crate::guest_import_authorization::{GuestOwnerRuntimeProfile, ObservedGuestRuntime};

const OWNER_NAME: &str = "ryeos-external-guest-occurrence-owner";
const MAX_OWNER_EXECUTABLE_BYTES: u64 = 256 * 1024 * 1024;

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
