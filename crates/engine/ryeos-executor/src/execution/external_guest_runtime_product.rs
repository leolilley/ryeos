//! Controller-owned staging of one retained guest-owner product for provider installation.
//!
//! This operation does not contact a provider or qualify a snapshot. It keeps
//! the product witness, controller root and exact re-observed bytes together
//! while producing a fresh private tree for a later descriptor-bound transfer.

use std::ffi::OsStr;
use std::io::Write as _;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution::guest_import_authorization::ObservedGuestRuntime;
use ryeos_external_execution::guest_runtime_product::{
    GuestOwnerRuntimeManifestIdentity, derive_guest_owner_runtime_manifest_identity,
};
use ryeos_state::external_content::products::transfer::ProductWitnessSource;
use ryeos_state::external_content::products::{ProductShape, ProductStorage};

use super::external_content::{PrivateMaterializationBudget, restore_workspace_output_tree};

const MAX_OWNER_SNAPSHOT_UPLOAD_BYTES: u64 = 64 * 1024 * 1024;

/// A fresh, descriptor-pinned tree. The caller must retain this value through
/// transfer; its portable authority is the witness and manifest, not its path.
pub struct StagedGuestOwnerRuntimeProduct {
    root: lillux::PinnedDirectory,
    root_identity: lillux::PinnedDirectoryIdentity,
    witness_hash: String,
    identity: GuestOwnerRuntimeManifestIdentity,
    maximum_bytes: u64,
}

/// Immutable Render directory-upload body. It remains only transport input;
/// the returned snapshot still requires independent restored-byte testimony.
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

impl StagedGuestOwnerRuntimeProduct {
    pub fn root(&self) -> &lillux::PinnedDirectory {
        &self.root
    }

    pub fn witness_hash(&self) -> &str {
        &self.witness_hash
    }

    pub fn identity(&self) -> &GuestOwnerRuntimeManifestIdentity {
        &self.identity
    }

    /// Recheck the pinned source immediately before a provider upload. A
    /// prior local observation cannot authorize bytes after filesystem drift.
    pub fn ensure_current(&self) -> Result<()> {
        ensure!(
            self.root.identity()? == self.root_identity,
            "staged guest runtime directory changed identity"
        );
        let current = ObservedGuestRuntime::observe(&self.root)?;
        ensure!(
            current.manifest_hash() == self.identity.manifest_hash,
            "staged guest runtime drifted before provider transfer"
        );
        Ok(())
    }

    /// Package exactly the admitted four-entry tree for Render's directory
    /// upload protocol. Content is read through pinned Lillux descriptors,
    /// never through a reconstructed CAS or ambient project path.
    pub fn sealed_snapshot_upload(&self) -> Result<GuestOwnerSnapshotUpload> {
        self.ensure_current()?;
        let mut archive = tar::Builder::new(Vec::new());
        append_directory(&mut archive, "bin/", 0o700)?;
        let bin = self
            .root
            .open_child_directory(OsStr::new("bin"))?
            .context("staged guest runtime has no bin directory")?;
        append_regular(
            &mut archive,
            &bin,
            "ryeos-external-guest-occurrence-owner",
            "bin/ryeos-external-guest-occurrence-owner",
            0o755,
            self.maximum_bytes,
        )?;
        append_regular(
            &mut archive,
            &self.root,
            "controller-root.hex",
            "controller-root.hex",
            0o644,
            64,
        )?;
        append_regular(
            &mut archive,
            &self.root,
            "guest-owner-profile.json",
            "guest-owner-profile.json",
            0o644,
            4 * 1024,
        )?;
        let uncompressed = archive.into_inner()?;
        ensure!(
            uncompressed.len() as u64 <= self.maximum_bytes.saturating_add(16 * 1024),
            "guest runtime archive exceeds its source bound"
        );
        let mut gzip = flate2::GzBuilder::new()
            .mtime(0)
            .write(Vec::new(), flate2::Compression::default());
        gzip.write_all(&uncompressed)?;
        let bytes = gzip.finish()?;
        ensure!(
            bytes.len() as u64 <= self.maximum_bytes.saturating_add(16 * 1024),
            "guest runtime compressed upload exceeds its source bound"
        );
        self.ensure_current()?;
        let descriptor = lillux::sealed_memfd(c"ryeos-guest-runtime-upload", &bytes)
            .map_err(anyhow::Error::msg)?;
        Ok(GuestOwnerSnapshotUpload {
            descriptor,
            bytes: bytes.len() as u64,
            sha256: lillux::sha256_hex(&bytes),
        })
    }
}

fn append_directory(archive: &mut tar::Builder<Vec<u8>>, path: &str, mode: u32) -> Result<()> {
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

fn append_regular(
    archive: &mut tar::Builder<Vec<u8>>,
    parent: &lillux::PinnedDirectory,
    name: &str,
    path: &str,
    mode: u32,
    maximum_bytes: u64,
) -> Result<()> {
    let file = parent
        .open_pinned_regular(OsStr::new(name), false)?
        .with_context(|| format!("staged guest runtime lacks {path}"))?;
    let observation = file.observation()?;
    ensure!(
        observation.portable_mode()? == mode,
        "staged guest runtime upload member changed portable mode"
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

/// Resolve a current, operator-owned product and copy its exact ordinary CAS
/// tree into an empty private child. Neither a caller-supplied filesystem path
/// nor an unverified manifest may select the bytes. Failure leaves the child
/// non-publishable for the caller to quarantine.
pub fn stage_current_guest_owner_runtime_product(
    state: &ryeos_app::state::AppState,
    context: &ryeos_app::handler_context::HandlerContext,
    witness_hash: &str,
    source: &ProductWitnessSource,
    maximum_bytes: u64,
    private_parent: &lillux::PinnedDirectory,
    child_name: &OsStr,
) -> Result<StagedGuestOwnerRuntimeProduct> {
    ryeos_app::operator_authority::require_admitted_operator(state, context)?;
    private_parent.require_owner_private_directory()?;
    ensure!(
        (1..=MAX_OWNER_SNAPSHOT_UPLOAD_BYTES).contains(&maximum_bytes),
        "guest runtime staging exceeds its fixed upload byte bound"
    );
    let limits = state
        .node_policy
        .require::<ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let witness =
        ryeos_app::operator_external_content::product_receipt::load_bounded_current_product_source(
            state,
            &authority,
            &guard,
            limits,
            &context.fingerprint,
            witness_hash,
            source,
            maximum_bytes,
        )?;
    ensure!(
        witness.evidence.declaration.shape == ProductShape::Tree
            && witness.evidence.declaration.storage == ProductStorage::Content
            && witness.evidence.total_bytes <= maximum_bytes,
        "guest runtime product is not a bounded ordinary tree"
    );
    let manifest = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        &authority.cas_store()?,
        &witness.evidence.manifest_hash,
        limits.max_object_bytes,
    )?;
    let identity =
        derive_guest_owner_runtime_manifest_identity(&manifest, state.identity.verifying_key())?;
    ensure!(
        identity.manifest_hash == witness.evidence.manifest_hash,
        "guest runtime product manifest contradicts its retained witness"
    );

    let root = private_parent.create_child(child_name, 0o700)?;
    restore_workspace_output_tree(
        &authority,
        &guard,
        &root,
        &identity.manifest_hash,
        ProductStorage::Content,
        &PrivateMaterializationBudget::new(maximum_bytes),
    )?;
    let observed = ObservedGuestRuntime::observe(&root)?;
    ensure!(
        observed.manifest_hash() == identity.manifest_hash,
        "staged guest runtime differs from the exact retained product"
    );
    Ok(StagedGuestOwnerRuntimeProduct {
        root_identity: root.identity()?,
        root,
        witness_hash: witness.attestation_hash,
        identity,
        maximum_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::SigningKey;
    use ryeos_external_execution::guest_import_authorization::GuestOwnerRuntimeProfile;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn staged_source_refuses_drift_before_provider_transfer() {
        let fixture = tempfile::tempdir().unwrap();
        let input = fixture.path().join("input");
        let output = fixture.path().join("output");
        std::fs::create_dir(&input).unwrap();
        std::fs::create_dir(&output).unwrap();
        std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o700)).unwrap();
        let owner_path = input.join("owner");
        let owner_bytes = b"exact-test-owner";
        std::fs::write(&owner_path, owner_bytes).unwrap();
        std::fs::set_permissions(&owner_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let input = lillux::PinnedDirectory::open(&input).unwrap().unwrap();
        let owner = input
            .open_pinned_regular(OsStr::new("owner"), false)
            .unwrap()
            .unwrap()
            .inherited_descriptor_authority()
            .unwrap();
        let output = lillux::PinnedDirectory::open(&output).unwrap().unwrap();
        let root_key = SigningKey::from_bytes(&[43; 32]).verifying_key();
        let product = ryeos_external_execution::guest_runtime_product::produce_guest_owner_runtime(
            &output,
            OsStr::new("runtime"),
            &owner,
            owner_bytes.len() as u64,
            &lillux::sha256_hex(owner_bytes),
            &root_key,
            &GuestOwnerRuntimeProfile {
                schema: 1,
                private_source_max_bytes: 32 * 1024 * 1024,
                private_source_max_inodes: 1024,
                owner_timeout_seconds: 600,
            },
        )
        .unwrap();
        let manifest = ryeos_state::observe_external_content_tree_exact(product.root()).unwrap();
        let identity = derive_guest_owner_runtime_manifest_identity(
            &serde_json::to_value(manifest).unwrap(),
            &root_key,
        )
        .unwrap();
        let staged = StagedGuestOwnerRuntimeProduct {
            root: product.root().try_clone().unwrap(),
            root_identity: product.root().identity().unwrap(),
            witness_hash: "a".repeat(64),
            identity,
            maximum_bytes: 1024 * 1024,
        };
        staged.ensure_current().unwrap();
        let package = staged.sealed_snapshot_upload().unwrap();
        let uploaded = lillux::read_sealed_inherited_descriptor(
            package.descriptor().inherited_descriptor().unwrap(),
            package.bytes() as usize,
        )
        .unwrap();
        assert_eq!(lillux::sha256_hex(&uploaded), package.sha256());
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(uploaded.as_slice()));
        let entries = tar
            .entries()
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                entry.path().unwrap().to_string_lossy().into_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            entries,
            [
                "bin/",
                "bin/ryeos-external-guest-occurrence-owner",
                "controller-root.hex",
                "guest-owner-profile.json",
            ]
        );
        std::fs::set_permissions(
            staged.root().path().join("guest-owner-profile.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert!(staged.ensure_current().is_err());
    }
}
