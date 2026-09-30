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
    EXTERNAL_CONTENT_MANIFEST_KIND, ExternalContentManifestEntryKind, ExternalContentManifestObject,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::guest_import_authorization::{GuestOwnerRuntimeProfile, ObservedGuestRuntime};

pub const OWNER_NAME: &str = "ryeos-external-guest-occurrence-owner";
const MAX_OWNER_EXECUTABLE_BYTES: u64 = 256 * 1024 * 1024;

/// Finite signed deployment recipe for assembling the owner runtime from one
/// exact bundle member. The serving node still authenticates the Config's
/// publisher, installed generation, target declaration and operator grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestOwnerMaterializationRecipe {
    pub schema: u32,
    pub guest_target_triple: String,
    pub maximum_owner_bytes: u64,
    pub profile: GuestOwnerRuntimeProfile,
}

impl GuestOwnerMaterializationRecipe {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1,
            "unsupported guest owner materialization recipe"
        );
        ensure!(
            !self.guest_target_triple.is_empty()
                && !self.guest_target_triple.starts_with('.')
                && !self.guest_target_triple.contains("..")
                && self.guest_target_triple.len() <= 96
                && self
                    .guest_target_triple
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
            "guest owner target triple is invalid"
        );
        ensure!(
            (1..=32 * 1024 * 1024).contains(&self.maximum_owner_bytes),
            "guest owner materialization byte bound is invalid"
        );
        self.profile.validate()?;
        Ok(())
    }

    pub fn owner_binary_ref(&self) -> Result<String> {
        self.validate()?;
        Ok(format!("bin/{}/{}", self.guest_target_triple, OWNER_NAME))
    }
}

/// Exact, immutable directory-upload body. A provider locator obtained after
/// using it remains unqualified until restored bytes are independently read.
pub struct GuestOwnerSnapshotUpload {
    descriptor: lillux::InheritedDescriptorAuthority,
    bytes: u64,
    sha256: String,
}

impl GuestOwnerSnapshotUpload {
    pub fn descriptor(&self) -> &lillux::InheritedDescriptorAuthority {
        &self.descriptor
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// Read only the four admitted members through pinned descriptors and produce
/// a deterministic plain tar. Observation before and after closes the mutable
/// tree window; the sealed descriptor is the sole upload source thereafter.
pub fn seal_guest_owner_snapshot_upload(
    root: &lillux::PinnedDirectory,
    expected_manifest_hash: &str,
    maximum_bytes: u64,
) -> Result<GuestOwnerSnapshotUpload> {
    ensure!(
        lillux::valid_hash(expected_manifest_hash)
            && (1..=64 * 1024 * 1024).contains(&maximum_bytes),
        "guest runtime snapshot upload has invalid source bounds"
    );
    let root_identity = root.identity()?;
    let observed = ObservedGuestRuntime::observe(root)?;
    ensure!(
        observed.manifest_hash() == expected_manifest_hash,
        "guest runtime upload differs from witnessed manifest"
    );
    let mut archive = tar::Builder::new(Vec::new());
    // The immutable runtime stays root-owned. The fixed non-root guest owner
    // may traverse and execute it, but cannot rewrite its public trust root or
    // executable after the snapshot is restored.
    append_snapshot_directory(&mut archive, "bin/", 0o755)?;
    let bin = root
        .open_child_directory(OsStr::new("bin"))?
        .context("guest runtime upload has no bin directory")?;
    append_snapshot_regular(
        &mut archive,
        &bin,
        "ryeos-external-guest-occurrence-owner",
        "bin/ryeos-external-guest-occurrence-owner",
        0o755,
        maximum_bytes,
    )?;
    append_snapshot_regular(
        &mut archive,
        root,
        "controller-root.hex",
        "controller-root.hex",
        0o644,
        64,
    )?;
    append_snapshot_regular(
        &mut archive,
        root,
        "guest-owner-profile.json",
        "guest-owner-profile.json",
        0o644,
        4 * 1024,
    )?;
    let bytes = archive.into_inner()?;
    ensure!(
        bytes.len() as u64 <= maximum_bytes.saturating_add(16 * 1024)
            && root.identity()? == root_identity
            && ObservedGuestRuntime::observe(root)?.manifest_hash() == expected_manifest_hash,
        "guest runtime upload changed during packaging"
    );
    let descriptor =
        lillux::sealed_memfd(c"ryeos-guest-runtime-upload", &bytes).map_err(anyhow::Error::msg)?;
    Ok(GuestOwnerSnapshotUpload {
        descriptor,
        bytes: bytes.len() as u64,
        sha256: lillux::sha256_hex(&bytes),
    })
}

fn append_snapshot_directory(
    archive: &mut tar::Builder<Vec<u8>>,
    path: &str,
    mode: u32,
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_size(0);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    archive.append_data(&mut header, path, std::io::empty())?;
    Ok(())
}

fn append_snapshot_regular(
    archive: &mut tar::Builder<Vec<u8>>,
    parent: &lillux::PinnedDirectory,
    name: &str,
    path: &str,
    mode: u32,
    maximum_bytes: u64,
) -> Result<()> {
    let file = parent
        .open_pinned_regular(OsStr::new(name), false)?
        .with_context(|| format!("guest runtime upload lacks {path}"))?;
    let observation = file.observation()?;
    ensure!(
        observation.portable_mode()? == mode,
        "guest runtime upload member changed portable mode"
    );
    let bytes = file.read_stable_bounded(&observation, maximum_bytes)?;
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(bytes.len() as u64);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    archive.append_data(&mut header, path, bytes.as_slice())?;
    Ok(())
}

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
    ensure!(
        value.get("kind").and_then(Value::as_str) == Some(EXTERNAL_CONTENT_MANIFEST_KIND),
        "guest runtime product requires the ordinary tree manifest observed by the guest"
    );
    let manifest = ExternalContentManifestObject::from_value(value)?;
    let entries = manifest
        .entries
        .iter()
        .map(|entry| EntryView {
            path: entry.path.clone(),
            kind: entry.kind,
            mode: entry.mode,
            sha256: entry.blob_hash.clone(),
            size: entry.size,
        })
        .collect::<Vec<_>>();
    ensure!(
        entries.len() == 4
            && entries.iter().any(|entry| {
                entry.path == "bin"
                    && entry.kind == ExternalContentManifestEntryKind::Dir
                    && entry.mode.is_none()
                    && entry.sha256.is_none()
                    && entry.size.is_none()
            }),
        "guest runtime product contains an unexpected entry or lacks its bin directory"
    );
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
    ensure!(
        entries.iter().all(|entry| {
            matches!(
                entry.path.as_str(),
                "bin"
                    | "bin/ryeos-external-guest-occurrence-owner"
                    | "controller-root.hex"
                    | "guest-owner-profile.json"
            )
        }),
        "guest runtime product contains an ambient entry"
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

/// Create a fresh runtime child under an admitted private parent from the
/// existing captured-execution producer's owned executable descriptor. The exact
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
    owner_executable.require_owned_executable()?;
    produce_guest_owner_runtime_from_exact_source(
        parent,
        name,
        owner_executable,
        owner_bytes,
        owner_sha256,
        controller_root,
        profile,
    )
}

/// Copy an independently admitted, sealed *nonexecutable* guest payload into
/// the same exact runtime tree. The caller must verify signed source authority
/// and the selected guest target before calling this operation.
pub fn produce_guest_owner_runtime_from_admitted_payload(
    parent: &lillux::PinnedDirectory,
    name: &OsStr,
    payload: &lillux::InheritedDescriptorAuthority,
    owner_bytes: u64,
    owner_sha256: &str,
    controller_root: &VerifyingKey,
    profile: &GuestOwnerRuntimeProfile,
) -> Result<GuestOwnerRuntimeProduct> {
    let source = payload.require_owned_regular()?;
    ensure!(
        source.mode() & 0o111 == 0,
        "guest payload source must be nonexecutable data"
    );
    produce_guest_owner_runtime_from_exact_source(
        parent,
        name,
        payload,
        owner_bytes,
        owner_sha256,
        controller_root,
        profile,
    )
}

fn produce_guest_owner_runtime_from_exact_source(
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
    owner_executable.require_owned_regular()?;
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
    fn materialization_recipe_selects_one_bounded_guest_member() {
        let recipe = GuestOwnerMaterializationRecipe {
            schema: 1,
            guest_target_triple: "x86_64-unknown-linux-musl".into(),
            maximum_owner_bytes: 32 * 1024 * 1024,
            profile: GuestOwnerRuntimeProfile {
                schema: 2,
                account: lillux::GuestRuntimeAccount::Unix {
                    uid: 65534,
                    gid: 65534,
                },
                private_source_max_bytes: 32 * 1024 * 1024,
                private_source_max_inodes: 1024,
                owner_timeout_seconds: 600,
            },
        };
        assert_eq!(
            recipe.owner_binary_ref().unwrap(),
            "bin/x86_64-unknown-linux-musl/ryeos-external-guest-occurrence-owner"
        );
        let mut changed = recipe.clone();
        changed.guest_target_triple = "../host".into();
        assert!(changed.validate().is_err());
        changed = recipe.clone();
        changed.maximum_owner_bytes = 64 * 1024 * 1024;
        assert!(changed.validate().is_err());
        let mut value = serde_json::to_value(recipe).unwrap();
        value["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<GuestOwnerMaterializationRecipe>(value).is_err());
    }

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
            schema: 2,
            account: lillux::GuestRuntimeAccount::Unix {
                uid: 65534,
                gid: 65534,
            },
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
        let observed_product = ObservedGuestRuntime::observe(product.root()).unwrap();
        assert_eq!(product.manifest_hash(), observed_product.manifest_hash());
        let ignore =
            ryeos_state::ignore::IgnoreMatcher::from_config(&ryeos_state::ignore::IgnoreConfig {
                patterns: vec![],
            })
            .unwrap();
        let capture_policy = ryeos_state::ExternalCapturePolicy::new(
            "products/external-guest-owner-runtime".to_owned(),
            &ignore,
        )
        .unwrap();
        let mut capture_budget = ryeos_state::LaunchCaptureBudget::bounded(
            3,
            4,
            32 * 1024 * 1024,
            32 * 1024 * 1024 + 4096,
        )
        .unwrap();
        let captured = ryeos_state::external_content::capture_tree(
            product.root(),
            &[],
            &capture_policy,
            &mut capture_budget,
            &mut ryeos_state::DigestOnlyExternalContentSink,
        )
        .unwrap();
        assert_eq!(
            ryeos_state::external_content_manifest_digest(&captured).unwrap(),
            product.manifest_hash()
        );
        let manifest = ryeos_state::observe_external_content_tree_exact(product.root()).unwrap();
        let manifest_value = serde_json::to_value(&manifest).unwrap();
        let identity = derive_guest_owner_runtime_manifest_identity(&manifest_value, &key).unwrap();
        assert_eq!(
            observed_product.measure_exact_owner_product().unwrap(),
            identity
        );
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
        let mut ambient = manifest_value.clone();
        ambient["entries"].as_array_mut().unwrap().insert(
            0,
            serde_json::json!({
                "path": "ambient.txt",
                "kind": "file",
                "mode": 420,
                "blob_hash": "a".repeat(64),
                "size": 1,
                "target": null
            }),
        );
        ambient["entry_count"] = serde_json::json!(5);
        ambient["total_bytes"] = serde_json::json!(manifest.total_bytes + 1);
        assert!(derive_guest_owner_runtime_manifest_identity(&ambient, &key).is_err());
        let mut large = manifest_value.clone();
        large["kind"] =
            serde_json::json!(ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND);
        large["schema"] = serde_json::json!(ryeos_state::objects::EXTERNAL_LARGE_CONTENT_SCHEMA);
        assert!(derive_guest_owner_runtime_manifest_identity(&large, &key).is_err());
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

    #[cfg(target_os = "linux")]
    #[test]
    fn sealed_nonexecutable_payload_produces_the_same_owner_runtime() {
        let parent = tempfile::tempdir().unwrap();
        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let parent = lillux::PinnedDirectory::open(parent.path())
            .unwrap()
            .unwrap();
        let bytes = b"signed-owner-payload";
        let payload = lillux::sealed_memfd(c"owner-payload-fixture", bytes).unwrap();
        assert!(payload.require_owned_executable().is_err());
        let key = SigningKey::from_bytes(&[43; 32]).verifying_key();
        let profile = GuestOwnerRuntimeProfile {
            schema: 2,
            account: lillux::GuestRuntimeAccount::Unix {
                uid: 65534,
                gid: 65534,
            },
            private_source_max_bytes: 32 * 1024 * 1024,
            private_source_max_inodes: 1024,
            owner_timeout_seconds: 600,
        };
        let product = produce_guest_owner_runtime_from_admitted_payload(
            &parent,
            OsStr::new("runtime"),
            &payload,
            bytes.len() as u64,
            &lillux::sha256_hex(bytes),
            &key,
            &profile,
        )
        .unwrap();
        let observed = ObservedGuestRuntime::observe(product.root()).unwrap();
        assert_eq!(observed.manifest_hash(), product.manifest_hash());
        assert_eq!(
            observed
                .measure_exact_owner_product()
                .unwrap()
                .owner_executable_sha256,
            lillux::sha256_hex(bytes)
        );
    }
}
