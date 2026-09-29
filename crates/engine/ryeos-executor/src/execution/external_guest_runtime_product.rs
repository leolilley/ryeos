//! Controller-owned staging of one retained guest-owner product for provider installation.
//!
//! This operation does not contact a provider or qualify a snapshot. It keeps
//! the product witness, controller root and exact re-observed bytes together
//! while producing a fresh private tree for a later descriptor-bound transfer.

use std::ffi::OsStr;

use anyhow::{Result, ensure};
use ryeos_external_execution::guest_import_authorization::ObservedGuestRuntime;
pub use ryeos_external_execution::guest_runtime_product::GuestOwnerSnapshotUpload;
use ryeos_external_execution::guest_runtime_product::{
    GuestOwnerRuntimeManifestIdentity, GuestOwnerRuntimeProduct,
    derive_guest_owner_runtime_manifest_identity,
    produce_guest_owner_runtime_from_admitted_payload, seal_guest_owner_snapshot_upload,
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

    /// Package exactly the admitted four-entry tree as an application/x-tar
    /// directory upload. Content is read through pinned Lillux descriptors,
    /// never through a reconstructed CAS or ambient project path.
    pub fn sealed_snapshot_upload(&self) -> Result<GuestOwnerSnapshotUpload> {
        self.ensure_current()?;
        seal_guest_owner_snapshot_upload(
            &self.root,
            &self.identity.manifest_hash,
            self.maximum_bytes,
        )
    }
}

/// A fresh private tree reconstructed only from a current, node-signed
/// materialization head and its exact retained CAS bytes. The source head is
/// not an execution-capture witness or a runtime qualification.
pub struct StagedGuestOwnerRuntimeMaterialization {
    root: lillux::PinnedDirectory,
    root_identity: lillux::PinnedDirectoryIdentity,
    binding_id: String,
    coordinate_digest: String,
    attestation_hash: String,
    identity: GuestOwnerRuntimeManifestIdentity,
    maximum_bytes: u64,
}

impl StagedGuestOwnerRuntimeMaterialization {
    pub fn root(&self) -> &lillux::PinnedDirectory {
        &self.root
    }

    pub fn attestation_hash(&self) -> &str {
        &self.attestation_hash
    }

    pub fn identity(&self) -> &GuestOwnerRuntimeManifestIdentity {
        &self.identity
    }

    pub fn ensure_current(&self) -> Result<()> {
        ensure!(
            self.root.identity()? == self.root_identity
                && ObservedGuestRuntime::observe(&self.root)?.manifest_hash()
                    == self.identity.manifest_hash,
            "staged materialized guest runtime drifted before provider transfer"
        );
        Ok(())
    }

    pub fn ensure_current_authority(
        &self,
        state: &ryeos_app::state::AppState,
        context: &ryeos_app::handler_context::HandlerContext,
    ) -> Result<()> {
        let authority = state.state_store.pinned_state_authority()?;
        let guard = authority.acquire_shared_guard()?;
        let current = ryeos_app::operator_guest_runtime_materialization::load_current_guest_owner_materialization(
            state,
            context,
            &self.binding_id,
            &self.coordinate_digest,
            &self.attestation_hash,
            &authority,
            &guard,
        )?;
        ensure!(
            current.identity == self.identity,
            "staged materialization authority changed after private reconstruction"
        );
        Ok(())
    }

    pub fn sealed_snapshot_upload(
        &self,
        state: &ryeos_app::state::AppState,
        context: &ryeos_app::handler_context::HandlerContext,
    ) -> Result<GuestOwnerSnapshotUpload> {
        self.ensure_current_authority(state, context)?;
        self.ensure_current()?;
        seal_guest_owner_snapshot_upload(
            &self.root,
            &self.identity.manifest_hash,
            self.maximum_bytes,
        )
    }
}

/// Stage a retained materialization without consulting the live Bundle path
/// for bytes. Current grant/binding/generation checks occur at head lookup;
/// snapshot production must repeat admission before provider contact.
#[allow(clippy::too_many_arguments)]
pub fn stage_current_guest_owner_runtime_materialization(
    state: &ryeos_app::state::AppState,
    context: &ryeos_app::handler_context::HandlerContext,
    materialization_binding_id: &str,
    coordinate_digest: &str,
    attestation_hash: &str,
    maximum_bytes: u64,
    private_parent: &lillux::PinnedDirectory,
    child_name: &OsStr,
) -> Result<StagedGuestOwnerRuntimeMaterialization> {
    private_parent.require_owner_private_directory()?;
    ensure!(
        (1..=MAX_OWNER_SNAPSHOT_UPLOAD_BYTES).contains(&maximum_bytes),
        "guest runtime staging exceeds its fixed upload byte bound"
    );
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let current = ryeos_app::operator_guest_runtime_materialization::load_current_guest_owner_materialization(
        state,
        context,
        materialization_binding_id,
        coordinate_digest,
        attestation_hash,
        &authority,
        &guard,
    )?;
    let cas = authority.cas_store()?;
    let source = &current.source;
    let product = produce_retained_guest_owner_tree(
        &cas,
        &source.owner_executable_sha256,
        &source.profile_digest,
        source.maximum_owner_bytes,
        maximum_bytes,
        &current.identity,
        state.identity.verifying_key(),
        private_parent,
        child_name,
    )?;
    let root = product.root().try_clone()?;
    Ok(StagedGuestOwnerRuntimeMaterialization {
        root_identity: root.identity()?,
        root,
        binding_id: materialization_binding_id.to_owned(),
        coordinate_digest: coordinate_digest.to_owned(),
        attestation_hash: current.attestation_hash,
        identity: current.identity,
        maximum_bytes,
    })
}

#[allow(clippy::too_many_arguments)]
fn produce_retained_guest_owner_tree(
    cas: &lillux::CasStore,
    owner_hash: &str,
    profile_hash: &str,
    maximum_owner_bytes: u64,
    maximum_staged_bytes: u64,
    expected: &GuestOwnerRuntimeManifestIdentity,
    controller_key: &lillux::crypto::VerifyingKey,
    private_parent: &lillux::PinnedDirectory,
    child_name: &OsStr,
) -> Result<GuestOwnerRuntimeProduct> {
    let owner = cas
        .get_blob_bounded(owner_hash, maximum_owner_bytes)?
        .ok_or_else(|| anyhow::anyhow!("materialization owner payload is missing from CAS"))?;
    ensure!(
        !owner.is_empty() && owner.len() as u64 <= maximum_staged_bytes,
        "materialization owner exceeds snapshot staging ceiling"
    );
    let profile_bytes = cas
        .get_blob_bounded(profile_hash, 4 * 1024)?
        .ok_or_else(|| anyhow::anyhow!("materialization profile is missing from CAS"))?;
    let profile: ryeos_external_execution::guest_import_authorization::GuestOwnerRuntimeProfile =
        serde_json::from_slice(&profile_bytes)?;
    ensure!(
        ryeos_external_execution_contract::canonical_json(&profile)? == profile_bytes,
        "retained materialization profile is not canonical"
    );
    ensure!(
        (owner.len() as u64)
            .checked_add(profile_bytes.len() as u64)
            .and_then(|total| total.checked_add(64))
            .is_some_and(|total| total <= maximum_staged_bytes),
        "materialization tree exceeds snapshot staging ceiling"
    );
    let sealed =
        lillux::sealed_memfd(c"ryeos-retained-guest-owner", &owner).map_err(anyhow::Error::msg)?;
    let product = produce_guest_owner_runtime_from_admitted_payload(
        private_parent,
        child_name,
        &sealed,
        owner.len() as u64,
        owner_hash,
        controller_key,
        &profile,
    )?;
    let root = product.root().try_clone()?;
    let observed = ObservedGuestRuntime::observe(&root)?;
    ensure!(
        observed.manifest_hash() == expected.manifest_hash
            && product.manifest_hash() == expected.manifest_hash,
        "staged materialized runtime differs from signed retained output"
    );
    Ok(product)
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
        let profile = GuestOwnerRuntimeProfile {
            schema: 1,
            private_source_max_bytes: 32 * 1024 * 1024,
            private_source_max_inodes: 1024,
            owner_timeout_seconds: 600,
        };
        let product = ryeos_external_execution::guest_runtime_product::produce_guest_owner_runtime(
            &output,
            OsStr::new("runtime"),
            &owner,
            owner_bytes.len() as u64,
            &lillux::sha256_hex(owner_bytes),
            &root_key,
            &profile,
        )
        .unwrap();
        let manifest = ryeos_state::observe_external_content_tree_exact(product.root()).unwrap();
        let identity = derive_guest_owner_runtime_manifest_identity(
            &serde_json::to_value(manifest).unwrap(),
            &root_key,
        )
        .unwrap();
        let cas = lillux::CasStore::new(fixture.path().join("cas"));
        let owner_hash = cas.store_blob(owner_bytes).unwrap();
        let profile_bytes = ryeos_external_execution_contract::canonical_json(&profile).unwrap();
        let profile_hash = cas.store_blob(&profile_bytes).unwrap();
        let retained = produce_retained_guest_owner_tree(
            &cas,
            &owner_hash,
            &profile_hash,
            32 * 1024 * 1024,
            1024 * 1024,
            &identity,
            &root_key,
            &output,
            OsStr::new("retained"),
        )
        .unwrap();
        assert_eq!(retained.manifest_hash(), identity.manifest_hash);
        let wrong_profile = "f".repeat(64);
        assert!(
            produce_retained_guest_owner_tree(
                &cas,
                &owner_hash,
                &wrong_profile,
                32 * 1024 * 1024,
                1024 * 1024,
                &identity,
                &root_key,
                &output,
                OsStr::new("unavailable"),
            )
            .is_err()
        );
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
        let mut tar = tar::Archive::new(uploaded.as_slice());
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
