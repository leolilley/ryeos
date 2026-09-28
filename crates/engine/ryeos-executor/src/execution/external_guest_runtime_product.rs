//! Controller-owned staging of one retained guest-owner product for provider installation.
//!
//! This operation does not contact a provider or qualify a snapshot. It keeps
//! the product witness, controller root and exact re-observed bytes together
//! while producing a fresh private tree for a later descriptor-bound transfer.

use std::ffi::OsStr;

use anyhow::{Result, ensure};
use ryeos_external_execution::guest_import_authorization::ObservedGuestRuntime;
use ryeos_external_execution::guest_runtime_product::{
    GuestOwnerRuntimeManifestIdentity, derive_guest_owner_runtime_manifest_identity,
};
use ryeos_state::external_content::products::transfer::ProductWitnessSource;
use ryeos_state::external_content::products::{ProductShape, ProductStorage};

use super::external_content::{PrivateMaterializationBudget, restore_workspace_output_tree};

/// A fresh, descriptor-pinned tree. The caller must retain this value through
/// transfer; its portable authority is the witness and manifest, not its path.
pub struct StagedGuestOwnerRuntimeProduct {
    root: lillux::PinnedDirectory,
    root_identity: lillux::PinnedDirectoryIdentity,
    witness_hash: String,
    identity: GuestOwnerRuntimeManifestIdentity,
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
        maximum_bytes > 0,
        "guest runtime staging requires a byte bound"
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
        };
        staged.ensure_current().unwrap();
        std::fs::set_permissions(
            staged.root().path().join("guest-owner-profile.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert!(staged.ensure_current().is_err());
    }
}
