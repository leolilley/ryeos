//! Source of the guest runtime upload for providers whose activation stages
//! the runtime itself.
//!
//! The sealed tar is not retained anywhere: only the runtime tree is, in the
//! content store, under the qualified product the placement contract names.
//! Rebuilding the tar from that tree needs the executor's staging machinery,
//! which this crate cannot depend on, so the composition root injects it
//! through `GuestRuntimeUploadSource`. This module owns everything around the
//! injected step: which product is staged, under whose authority, and that the
//! result can leave the node.

use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use ryeos_external_execution::guest_runtime_product::GuestOwnerSnapshotUpload;
use ryeos_external_execution_contract::MAX_GUEST_SOURCE_BYTES;
use ryeos_state::external_content::products::transfer::ProductWitnessSource;
use ryeos_state::external_content::qualification_purpose::QualificationSubject;

use crate::handler_context::HandlerContext;
use crate::node_config::sections::external_execution::{
    ExternalPlacementBackendContract, ExternalRuntimeQualificationBinding,
};
use crate::operator_external_content::product_qualification::PreparedExternalQualificationRoot;
use crate::state::AppState;
use crate::state_store::RuntimeSnapshotStageLease;

/// One retained runtime product to turn into a sealed upload.
pub struct GuestRuntimeUploadRequest<'a> {
    /// Attestation of the current product witness holding the runtime tree.
    pub witness_hash: &'a str,
    pub witness_source: &'a ProductWitnessSource,
    /// Manifest hash the staged tree and the sealed tar must both equal.
    pub manifest_hash: &'a str,
    pub maximum_bytes: u64,
    /// Private scratch the source stages the tree into. It outlives the call,
    /// and the returned upload holds its own sealed copy of the bytes.
    pub stage: &'a RuntimeSnapshotStageLease,
}

pub trait GuestRuntimeUploadSource: Send + Sync + std::fmt::Debug {
    /// Stage the exact retained product, recheck it is unchanged, and seal
    /// its deterministic tar. The tar must be the one whose tree has
    /// `request.manifest_hash`; anything else is an error.
    fn produce(
        &self,
        state: &AppState,
        context: &HandlerContext,
        request: &GuestRuntimeUploadRequest<'_>,
    ) -> Result<GuestOwnerSnapshotUpload>;
}

/// The injected source together with the node state it produces against.
#[derive(Clone)]
pub(crate) struct GuestRuntimeUploadBinding {
    source: Arc<dyn GuestRuntimeUploadSource>,
    state: AppState,
}

impl GuestRuntimeUploadBinding {
    pub(crate) fn new(source: Arc<dyn GuestRuntimeUploadSource>, state: &AppState) -> Self {
        Self {
            source,
            state: state.clone(),
        }
    }

    /// Produce the upload for the runtime product the activation's basis
    /// names. Offline: the product is read from the content store and nothing
    /// is contacted.
    pub(crate) fn produce(
        &self,
        contract: &ExternalPlacementBackendContract,
        basis: GuestRuntimeBasis<'_>,
    ) -> Result<GuestOwnerSnapshotUpload> {
        let product = match basis {
            GuestRuntimeBasis::Ordinary => {
                let qualification = ordinary_runtime_qualification(contract)?;
                // A remote testimony names the receiver's own received-product
                // source; the origin's evidence names its local capture.
                let (witness_hash, witness_source) = if qualification.remote_testimony.is_some() {
                    let remote =
                        crate::operator_external_content::qualification_testimony::verify_current_remote_runtime_testimony(
                            &self.state,
                            qualification,
                            &contract.guest_runtime_manifest_hash,
                        )?;
                    (
                        remote.evidence.product_witness_hash,
                        remote.acceptance.product_source,
                    )
                } else {
                    let witness =
                        crate::operator_external_content::product_qualification::verify_current_external_runtime_qualification(
                            &self.state,
                            qualification,
                            &contract.guest_runtime_manifest_hash,
                        )?;
                    (
                        witness.evidence.product_witness_hash,
                        witness.evidence.witness_source,
                    )
                };
                RuntimeProduct {
                    owner_principal: qualification.owner_principal.clone(),
                    witness_hash,
                    witness_source,
                }
            }
            GuestRuntimeBasis::QualificationCandidate(root) => {
                let purpose = root.purpose();
                candidate_runtime_product(
                    contract,
                    &purpose.subject,
                    &purpose.owner_fingerprint,
                    &purpose.subject_manifest_hash,
                )?
            }
        };
        let context = crate::operator_authority::admitted_operator_authority_for_principal(
            &self.state,
            &product.owner_principal,
        )?
        .handler_context();
        let maximum_bytes = contract.max_transfer_bytes.min(MAX_GUEST_SOURCE_BYTES);
        let stage = self.state.state_store.begin_runtime_snapshot_stage()?;
        let upload = self.source.produce(
            &self.state,
            &context,
            &GuestRuntimeUploadRequest {
                witness_hash: &product.witness_hash,
                witness_source: &product.witness_source,
                manifest_hash: &contract.guest_runtime_manifest_hash,
                maximum_bytes,
                stage: &stage,
            },
        )?;
        require_deliverable_upload(
            &crate::private_artifact_home::root(&self.state.config.runtime_state_dir()),
            &upload,
            maximum_bytes,
        )?;
        Ok(upload)
    }
}

/// What the activation is for, which fixes where the runtime product comes
/// from. Every other basis has no variant and is refused by the caller.
pub(crate) enum GuestRuntimeBasis<'a> {
    /// The product the node's signed binding qualified for ordinary use.
    Ordinary,
    /// The product the live qualification session is itself qualifying; it
    /// has no qualified product of its own yet.
    QualificationCandidate(&'a PreparedExternalQualificationRoot),
}

#[derive(Debug)]
struct RuntimeProduct {
    owner_principal: String,
    witness_hash: String,
    witness_source: ProductWitnessSource,
}

fn ordinary_runtime_qualification(
    contract: &ExternalPlacementBackendContract,
) -> Result<&ExternalRuntimeQualificationBinding> {
    contract.runtime_qualification.as_ref().context(
        "provider stages its guest runtime but the signed placement binding \
         selects no runtime qualification",
    )
}

/// The qualification session fixes the exact product being qualified and its
/// manifest. That manifest must be the one the contract (and so the signed
/// import and the guest launcher) names, or the staged tree could never be
/// admitted.
fn candidate_runtime_product(
    contract: &ExternalPlacementBackendContract,
    subject: &QualificationSubject,
    owner_fingerprint: &str,
    subject_manifest_hash: &str,
) -> Result<RuntimeProduct> {
    let QualificationSubject::CapturedProduct {
        product_witness_hash,
        witness_source,
        ..
    } = subject
    else {
        bail!("a qualification over activated content has no captured guest runtime to stage");
    };
    ensure!(
        subject_manifest_hash == contract.guest_runtime_manifest_hash,
        "qualification subject is not the guest runtime the placement contract names"
    );
    Ok(RuntimeProduct {
        owner_principal: format!("fp:{owner_fingerprint}"),
        witness_hash: product_witness_hash.clone(),
        witness_source: witness_source.clone(),
    })
}

impl std::fmt::Debug for GuestRuntimeUploadBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuestRuntimeUploadBinding")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

/// Refuse, before any claim, an upload that cannot cross to the provider: one
/// sourced from a credential home, or one outside the contract's byte bound.
pub(crate) fn require_deliverable_upload(
    credential_home_root: &std::path::Path,
    upload: &GuestOwnerSnapshotUpload,
    maximum_bytes: u64,
) -> Result<()> {
    ensure!(
        (1..=maximum_bytes).contains(&upload.bytes()),
        "guest runtime upload exceeds the placement transfer bound"
    );
    crate::external_lifecycle_adapter::refuse_credential_home_upload(
        credential_home_root,
        upload.descriptor(),
    )
}

#[cfg(test)]
pub(crate) fn test_upload(manifest_scratch: &std::path::Path) -> GuestOwnerSnapshotUpload {
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt as _;

    use ryeos_external_execution::guest_import_authorization::GuestOwnerRuntimeProfile;

    let input = manifest_scratch.join("input");
    let output = manifest_scratch.join("output");
    std::fs::create_dir(&input).unwrap();
    std::fs::create_dir(&output).unwrap();
    std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o700)).unwrap();
    let owner_bytes = b"exact-test-owner";
    let owner_path = input.join("owner");
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
    let root_key = lillux::crypto::SigningKey::from_bytes(&[43; 32]).verifying_key();
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
    let manifest_hash =
        ryeos_external_execution::guest_import_authorization::ObservedGuestRuntime::observe(
            product.root(),
        )
        .unwrap()
        .manifest_hash()
        .to_owned();
    ryeos_external_execution::guest_runtime_product::seal_guest_owner_snapshot_upload(
        product.root(),
        &manifest_hash,
        1024 * 1024,
    )
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_config::sections::external_execution::InstalledExternalExecutionBinding;

    fn contract() -> ExternalPlacementBackendContract {
        InstalledExternalExecutionBinding::test_fixture().backend_contract()
    }

    #[test]
    fn an_ordinary_activation_with_no_runtime_qualification_is_refused() {
        let mut contract = contract();
        contract.runtime_qualification = None;
        let refusal = ordinary_runtime_qualification(&contract).unwrap_err();
        assert!(
            format!("{refusal:#}").contains("selects no runtime qualification"),
            "{refusal:#}"
        );
    }

    #[test]
    fn a_candidate_activation_stages_exactly_the_contracts_captured_runtime() {
        let contract = contract();
        let subject = QualificationSubject::CapturedProduct {
            product_witness_hash: "a".repeat(64),
            witness_source: ProductWitnessSource::LocalCapture {},
            relationship_name: "runtime".into(),
        };
        let product = candidate_runtime_product(
            &contract,
            &subject,
            &"b".repeat(64),
            &contract.guest_runtime_manifest_hash,
        )
        .unwrap();
        assert_eq!(product.owner_principal, format!("fp:{}", "b".repeat(64)));
        assert_eq!(product.witness_hash, "a".repeat(64));
        assert_eq!(
            product.witness_source,
            ProductWitnessSource::LocalCapture {}
        );

        let other = "f".repeat(64);
        assert_ne!(other, contract.guest_runtime_manifest_hash);
        let refusal =
            candidate_runtime_product(&contract, &subject, &"b".repeat(64), &other).unwrap_err();
        assert!(
            format!("{refusal:#}").contains("not the guest runtime the placement contract names"),
            "{refusal:#}"
        );
    }

    #[test]
    fn an_upload_over_its_bound_or_from_a_credential_home_is_not_deliverable() {
        let scratch = tempfile::tempdir().unwrap();
        let upload = test_upload(scratch.path());
        let homes = tempfile::tempdir().unwrap();
        require_deliverable_upload(homes.path(), &upload, upload.bytes()).unwrap();
        let over = require_deliverable_upload(homes.path(), &upload, upload.bytes() - 1);
        assert!(
            format!("{:#}", over.unwrap_err()).contains("exceeds the placement transfer bound")
        );
        assert!(require_deliverable_upload(homes.path(), &upload, 0).is_err());
    }
}
