//! Exact controller-to-guest admission before one-shot package import.
//!
//! The trusted root must come from the qualified installed guest runtime.
//! A separate root-signed assignment delegates the per-occurrence owner key;
//! neither that key nor the assignment may be inferred from the upload.

use anyhow::{Context as _, Result, ensure};
use lillux::crypto::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use ryeos_external_execution_contract::guest_import_authorization::{
    GuestImportAuthorization, GuestOccurrenceAssignment, GuestOccurrenceAssignmentDocument,
    MAX_GUEST_IMPORT_AUTHORIZATION_BYTES, MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES,
    SignedGuestImportAuthorization, SignedGuestOccurrenceAssignment,
};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;

use crate::guest_runtime_product::{
    GuestOwnerRuntimeManifestIdentity, derive_guest_owner_runtime_manifest_identity,
};

const CONTROLLER_ROOT_FILE: &str = "controller-root.hex";
const OWNER_PROFILE_FILE: &str = "guest-owner-profile.json";
const MAX_OWNER_PROFILE_BYTES: u64 = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestOwnerRuntimeProfile {
    pub schema: u32,
    pub private_source_max_bytes: u64,
    pub private_source_max_inodes: u64,
    pub owner_timeout_seconds: u32,
}

impl GuestOwnerRuntimeProfile {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1,
            "unsupported installed guest-owner profile"
        );
        self.private_source_limits()
            .validate()
            .map_err(anyhow::Error::msg)?;
        ensure!(
            (1..=5_400).contains(&self.owner_timeout_seconds),
            "installed guest-owner timeout exceeds runtime ceiling"
        );
        Ok(())
    }

    pub fn private_source_limits(&self) -> lillux::sandbox::LinuxPrivateSourceLimits {
        lillux::sandbox::LinuxPrivateSourceLimits {
            max_bytes: self.private_source_max_bytes,
            max_inodes: self.private_source_max_inodes,
        }
    }

    pub fn owner_timeout_seconds(&self) -> f64 {
        f64::from(self.owner_timeout_seconds)
    }
}

/// Exact point observation of an installed guest runtime. The provider
/// snapshot-to-runtime qualification and writer exclusion remain separate;
/// this type never promotes a mutable directory to trusted installed content.
pub struct ObservedGuestRuntime {
    root: lillux::PinnedDirectory,
    root_identity: lillux::PinnedDirectoryIdentity,
    manifest_hash: String,
    controller_root: VerifyingKey,
    profile: GuestOwnerRuntimeProfile,
}

impl ObservedGuestRuntime {
    pub fn observe(root: &lillux::PinnedDirectory) -> Result<Self> {
        let root_identity = root.identity()?;
        let manifest = ryeos_state::observe_external_content_tree_exact(root)?;
        let manifest_hash = ryeos_state::external_content_manifest_digest(&manifest)?;
        let root_file = root
            .open_pinned_regular(OsStr::new(CONTROLLER_ROOT_FILE), false)?
            .context("installed guest runtime has no controller root")?;
        ensure!(
            matches!(root_file.permission_mode()?, 0o444 | 0o644),
            "installed controller root has unexpected file mode"
        );
        let observation = root_file.observation()?;
        ensure!(
            observation.size() == 64,
            "installed controller root has wrong length"
        );
        let bytes = root_file.read_stable_bounded(&observation, 64)?;
        let root_bytes: [u8; 32] = hex::decode(&bytes)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("installed controller root changed length"))?;
        let controller_root = VerifyingKey::from_bytes(&root_bytes)?;
        ensure!(
            !controller_root.is_weak(),
            "installed controller root is weak"
        );
        let profile_file = root
            .open_pinned_regular(OsStr::new(OWNER_PROFILE_FILE), false)?
            .context("installed guest runtime has no owner profile")?;
        ensure!(
            matches!(profile_file.permission_mode()?, 0o444 | 0o644),
            "installed guest-owner profile has unexpected file mode"
        );
        let profile_observation = profile_file.observation()?;
        ensure!(
            profile_observation.size() <= MAX_OWNER_PROFILE_BYTES,
            "installed guest-owner profile exceeds byte bound"
        );
        let profile_bytes =
            profile_file.read_stable_bounded(&profile_observation, MAX_OWNER_PROFILE_BYTES)?;
        let profile: GuestOwnerRuntimeProfile =
            ryeos_external_execution_contract::from_json_slice_strict(
                &profile_bytes,
                MAX_OWNER_PROFILE_BYTES as usize,
            )?;
        profile.validate()?;
        ensure!(
            ryeos_external_execution_contract::canonical_json(&profile)? == profile_bytes,
            "installed guest-owner profile is noncanonical"
        );
        Ok(Self {
            root: root.try_clone()?,
            root_identity,
            manifest_hash,
            controller_root,
            profile,
        })
    }

    pub fn manifest_hash(&self) -> &str {
        &self.manifest_hash
    }

    /// Strict four-entry owner-product measurement for a restored snapshot.
    /// This is intentionally separate from generic installed-runtime
    /// observation and grants no provider or snapshot identity by itself.
    pub fn measure_exact_owner_product(&self) -> Result<GuestOwnerRuntimeManifestIdentity> {
        ensure!(
            self.root.identity()? == self.root_identity,
            "installed runtime root changed before owner-product measurement"
        );
        let manifest = ryeos_state::observe_external_content_tree_exact(&self.root)?;
        ensure!(
            ryeos_state::external_content_manifest_digest(&manifest)? == self.manifest_hash,
            "installed runtime changed before owner-product measurement"
        );
        let identity = derive_guest_owner_runtime_manifest_identity(
            &serde_json::to_value(&manifest)?,
            &self.controller_root,
        )?;
        ensure!(
            identity.manifest_hash == self.manifest_hash,
            "owner-product measurement changed manifest identity"
        );
        Ok(identity)
    }

    pub fn profile(&self) -> &GuestOwnerRuntimeProfile {
        &self.profile
    }

    /// A mutable occurrence journal or source may not become an ambient
    /// entry beneath the exact runtime tree used as the controller-root anchor.
    pub fn require_disjoint_directory_tree(&self, other: &lillux::PinnedDirectory) -> Result<()> {
        self.root.require_disjoint_directory_tree(other)?;
        Ok(())
    }

    /// Refuse drift immediately before admitting the exact one-shot import.
    /// This does not prove that Render supplied the qualified snapshot or that
    /// the installed tree cannot change after this point.
    pub fn verify_import_documents(
        &self,
        signed_import_bytes: &[u8],
        signed_assignment_bytes: &[u8],
    ) -> Result<VerifiedGuestImportAuthorization> {
        ensure!(
            self.root.identity()? == self.root_identity,
            "installed guest runtime root changed identity"
        );
        let current = Self::observe(&self.root)?;
        ensure!(
            current.manifest_hash == self.manifest_hash
                && current.controller_root == self.controller_root
                && current.profile == self.profile,
            "installed guest runtime drifted before import admission"
        );
        verify_guest_import_documents(
            signed_import_bytes,
            &self.controller_root,
            &self.manifest_hash,
            signed_assignment_bytes,
        )
    }
}

/// Root-authenticated delegation of the exact per-occurrence owner key.
/// The root key must be pinned by the qualified guest runtime, not supplied
/// alongside the signed assignment by the provider or uploaded package.
pub struct VerifiedGuestOccurrenceAssignment {
    assignment: GuestOccurrenceAssignmentDocument,
    owner_key: VerifyingKey,
}

impl VerifiedGuestOccurrenceAssignment {
    pub fn assignment(&self) -> GuestOccurrenceAssignment<'_> {
        self.assignment.borrowed()
    }

    pub fn owner_key(&self) -> &VerifyingKey {
        &self.owner_key
    }
}

pub fn sign_guest_occurrence_assignment(
    assignment: GuestOccurrenceAssignmentDocument,
    controller_root: &SigningKey,
) -> Result<SignedGuestOccurrenceAssignment> {
    assignment.validate_shape()?;
    let signature = controller_root.sign(&assignment.signing_bytes()?);
    Ok(SignedGuestOccurrenceAssignment {
        assignment,
        signature_hex: hex::encode(signature.to_bytes()),
    })
}

pub fn verify_signed_guest_occurrence_assignment(
    signed: SignedGuestOccurrenceAssignment,
    trusted_root: &VerifyingKey,
) -> Result<VerifiedGuestOccurrenceAssignment> {
    signed.validate_shape()?;
    let signature_bytes: [u8; 64] = hex::decode(&signed.signature_hex)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("guest assignment signature changed length"))?;
    trusted_root
        .verify(
            &signed.assignment.signing_bytes()?,
            &Signature::from_bytes(&signature_bytes),
        )
        .context("guest assignment was not signed by its installed controller root")?;
    let owner_bytes: [u8; 32] = hex::decode(&signed.assignment.owner_public_key_hex)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("guest owner key changed length"))?;
    let owner_key = VerifyingKey::from_bytes(&owner_bytes)?;
    ensure!(!owner_key.is_weak(), "guest owner key is weak");
    Ok(VerifiedGuestOccurrenceAssignment {
        assignment: signed.assignment,
        owner_key,
    })
}

/// A successful signature and protected-assignment join. The private fields
/// prevent callers from constructing a token out of an uploaded ticket alone.
pub struct VerifiedGuestImportAuthorization {
    authorization: GuestImportAuthorization,
    identity_digest: String,
}

impl VerifiedGuestImportAuthorization {
    pub fn authorization(&self) -> &GuestImportAuthorization {
        &self.authorization
    }

    pub fn identity_digest(&self) -> &str {
        &self.identity_digest
    }

    pub fn require_fresh_admission(&self) -> Result<()> {
        self.require_fresh_admission_at(lillux::time::timestamp_millis())
    }

    pub(crate) fn require_fresh_admission_at(&self, now_ms: i64) -> Result<()> {
        self.authorization.require_fresh_admission_at(now_ms)
    }
}

fn verify_guest_import_authorization(
    signed: SignedGuestImportAuthorization,
    trusted_controller: &VerifyingKey,
    assignment: &GuestOccurrenceAssignment<'_>,
) -> Result<VerifiedGuestImportAuthorization> {
    signed.validate_shape()?;
    signed.authorization.validate_for_assignment(assignment)?;
    let signature_bytes: [u8; 64] = hex::decode(&signed.signature_hex)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("guest import signature changed length"))?;
    let signed_bytes = signed.authorization.signing_bytes()?;
    trusted_controller
        .verify(&signed_bytes, &Signature::from_bytes(&signature_bytes))
        .context("guest import authorization signature differs from trusted controller")?;
    Ok(VerifiedGuestImportAuthorization {
        identity_digest: signed.authorization.identity_digest()?,
        authorization: signed.authorization,
    })
}

/// Parse and join the two different input authorities at the guest importer.
/// The caller supplies the root key from its independently qualified runtime,
/// a root-signed assignment through its placement channel, and the separately
/// delivered signed import. Neither document can define the trusted root.
pub fn verify_guest_import_documents(
    signed_bytes: &[u8],
    trusted_root: &VerifyingKey,
    installed_guest_runtime_manifest_hash: &str,
    signed_assignment_bytes: &[u8],
) -> Result<VerifiedGuestImportAuthorization> {
    ensure!(
        !signed_assignment_bytes.is_empty()
            && signed_assignment_bytes.len() <= MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES
            && !signed_bytes.is_empty()
            && signed_bytes.len() <= MAX_GUEST_IMPORT_AUTHORIZATION_BYTES + 256,
        "guest import document exceeds its input bound"
    );
    let signed_assignment: SignedGuestOccurrenceAssignment =
        serde_json::from_slice(signed_assignment_bytes)
            .context("parse independently supplied signed guest assignment")?;
    signed_assignment.validate_shape()?;
    ensure!(
        ryeos_external_execution_contract::canonical_json(&signed_assignment)?
            == signed_assignment_bytes,
        "signed guest occurrence assignment is not canonical"
    );
    let assignment = verify_signed_guest_occurrence_assignment(signed_assignment, trusted_root)?;
    ensure!(
        assignment.assignment().guest_runtime_manifest_hash
            == installed_guest_runtime_manifest_hash,
        "signed guest assignment changed the installed runtime identity"
    );
    let signed: SignedGuestImportAuthorization =
        serde_json::from_slice(signed_bytes).context("parse signed guest import authorization")?;
    signed.validate_shape()?;
    ensure!(
        ryeos_external_execution_contract::canonical_json(&signed)? == signed_bytes,
        "signed guest import authorization is not canonical"
    );
    verify_guest_import_authorization(signed, assignment.owner_key(), &assignment.assignment())
}

/// The controller signs only an already checked, exact occurrence assignment.
/// A guest must still verify against its independently provisioned key and
/// assignment before creating the durable import owner.
pub fn sign_guest_import_authorization(
    authorization: GuestImportAuthorization,
    controller: &SigningKey,
    assignment: &GuestOccurrenceAssignment<'_>,
) -> Result<SignedGuestImportAuthorization> {
    sign_guest_import_authorization_at(
        authorization,
        controller,
        assignment,
        lillux::time::timestamp_millis(),
    )
}

fn sign_guest_import_authorization_at(
    authorization: GuestImportAuthorization,
    controller: &SigningKey,
    assignment: &GuestOccurrenceAssignment<'_>,
    now_ms: i64,
) -> Result<SignedGuestImportAuthorization> {
    authorization.validate_for_assignment(assignment)?;
    authorization.require_fresh_admission_at(now_ms)?;
    let signature = controller.sign(&authorization.signing_bytes()?);
    Ok(SignedGuestImportAuthorization {
        authorization,
        signature_hex: hex::encode(signature.to_bytes()),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use crate::guest_installation::{
        GuestOccurrenceOwner, GuestOccurrenceRecoveryPhase, recover_guest_occurrence_authorized,
    };
    use ryeos_external_execution_contract::guest_import_authorization::{
        GUEST_IMPORT_AUTHORIZATION_SCHEMA, GUEST_OCCURRENCE_ASSIGNMENT_SCHEMA,
    };
    use ryeos_external_execution_contract::staging_package::{
        GUEST_IMPORT_TICKET_SCHEMA, GuestImportTicket,
    };
    use ryeos_external_execution_contract::{
        EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA, GuestBaseSnapshotInput, GuestMountAccess,
        GuestMountContentAuthority, GuestMountInput, GuestMountKind, GuestMountRole,
    };

    fn fixture() -> (GuestImportAuthorization, GuestOccurrenceAssignment<'static>) {
        let digest = |byte: char| byte.to_string().repeat(64);
        let inputs = ryeos_external_execution_contract::ExternalGuestInputProjection {
            schema: EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            base_snapshot: GuestBaseSnapshotInput {
                descriptor: 56,
                snapshot_hash: digest('a'),
                closure_digest: digest('b'),
                object_count: 3,
                blob_count: 1,
                total_bytes: 1,
            },
            workspace_outputs: None,
            inputs: vec![GuestMountInput {
                role: GuestMountRole::Configuration,
                authority_id: "configuration".into(),
                descriptor: 64,
                destination: "/runtime/configuration".into(),
                kind: GuestMountKind::RegularFile,
                access: GuestMountAccess::ReadOnly,
                normalized_mode: Some(0o644),
                content_authority: GuestMountContentAuthority::RawFile {
                    sha256: digest('c'),
                },
                bytes: 1,
            }],
            executable_search: Vec::new(),
            environment: BTreeMap::new(),
        };
        let ticket = GuestImportTicket {
            schema: GUEST_IMPORT_TICKET_SCHEMA,
            binding_hash: digest('d'),
            allocation_request_digest: digest('e'),
            occurrence_id: "occ-1".into(),
            activation_request_digest: digest('f'),
            guest_input_identity: inputs.identity_digest().unwrap(),
            payload_sha256: digest('0'),
            manifest_sha256: digest('1'),
            framed_bytes: 200,
            regular_bytes: 100,
            bootstrap_sha256: digest('2'),
            supervisor_sha256: digest('3'),
            launcher_sha256: digest('4'),
            maximum_regular_bytes: 1000,
            maximum_framed_bytes: 2000,
        };
        (
            GuestImportAuthorization {
                schema: GUEST_IMPORT_AUTHORIZATION_SCHEMA,
                placement_thread_id: "T-guest-1".into(),
                admitted_capsule_hash: digest('5'),
                base_snapshot_hash: digest('a'),
                execution_binding_hash: digest('d'),
                allocation_request_digest: digest('e'),
                occurrence_id: "occ-1".into(),
                activation_request_digest: digest('f'),
                supervisor_runtime_hash: digest('6'),
                guest_runtime_manifest_hash: digest('7'),
                attachment_deadline_ms: 20_000,
                admission_deadline_ms: 10_000,
                nonce_sha256: digest('8'),
                ticket,
                guest_inputs: inputs,
            },
            GuestOccurrenceAssignment {
                placement_thread_id: "T-guest-1",
                admitted_capsule_hash: "5555555555555555555555555555555555555555555555555555555555555555",
                base_snapshot_hash: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                execution_binding_hash: "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                allocation_request_digest: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                occurrence_id: "occ-1",
                activation_request_digest: "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                supervisor_runtime_hash: "6666666666666666666666666666666666666666666666666666666666666666",
                guest_runtime_manifest_hash: "7777777777777777777777777777777777777777777777777777777777777777",
                attachment_deadline_ms: 20_000,
            },
        )
    }

    #[test]
    fn installed_runtime_observation_rechecks_root_and_all_content_before_import() {
        let directory = tempfile::tempdir().unwrap();
        let root_key = SigningKey::from_bytes(&[43; 32]);
        let root_file = directory.path().join(CONTROLLER_ROOT_FILE);
        std::fs::write(&root_file, hex::encode(root_key.verifying_key().to_bytes())).unwrap();
        let profile_file = directory.path().join(OWNER_PROFILE_FILE);
        std::fs::write(
            &profile_file,
            ryeos_external_execution_contract::canonical_json(&GuestOwnerRuntimeProfile {
                schema: 1,
                private_source_max_bytes: 32 * 1024 * 1024,
                private_source_max_inodes: 1024,
                owner_timeout_seconds: 10,
            })
            .unwrap(),
        )
        .unwrap();
        let runtime = lillux::PinnedDirectory::open(directory.path())
            .unwrap()
            .unwrap();
        let observed = ObservedGuestRuntime::observe(&runtime).unwrap();
        assert!(observed.measure_exact_owner_product().is_err());
        let separate = tempfile::tempdir().unwrap();
        let separate = lillux::PinnedDirectory::open(separate.path())
            .unwrap()
            .unwrap();
        observed.require_disjoint_directory_tree(&separate).unwrap();
        let nested = runtime
            .create_child(OsStr::new("nested-mutable"), 0o700)
            .unwrap();
        assert!(observed.require_disjoint_directory_tree(&nested).is_err());
        drop(nested);
        std::fs::remove_dir(directory.path().join("nested-mutable")).unwrap();
        let (mut authorization, original_assignment) = fixture();
        let manifest_hash = observed.manifest_hash().to_owned();
        authorization.guest_runtime_manifest_hash = manifest_hash.clone();
        let assignment = GuestOccurrenceAssignment {
            guest_runtime_manifest_hash: &manifest_hash,
            ..original_assignment
        };
        let owner_key = SigningKey::from_bytes(&[41; 32]);
        let signed_import =
            sign_guest_import_authorization_at(authorization, &owner_key, &assignment, 1).unwrap();
        let signed_assignment = sign_guest_occurrence_assignment(
            GuestOccurrenceAssignmentDocument {
                schema: GUEST_OCCURRENCE_ASSIGNMENT_SCHEMA,
                placement_thread_id: assignment.placement_thread_id.into(),
                admitted_capsule_hash: assignment.admitted_capsule_hash.into(),
                base_snapshot_hash: assignment.base_snapshot_hash.into(),
                execution_binding_hash: assignment.execution_binding_hash.into(),
                allocation_request_digest: assignment.allocation_request_digest.into(),
                occurrence_id: assignment.occurrence_id.into(),
                activation_request_digest: assignment.activation_request_digest.into(),
                supervisor_runtime_hash: assignment.supervisor_runtime_hash.into(),
                guest_runtime_manifest_hash: assignment.guest_runtime_manifest_hash.into(),
                owner_public_key_hex: hex::encode(owner_key.verifying_key().to_bytes()),
                attachment_deadline_ms: assignment.attachment_deadline_ms,
            },
            &root_key,
        )
        .unwrap();
        let import_bytes =
            ryeos_external_execution_contract::canonical_json(&signed_import).unwrap();
        let assignment_bytes =
            ryeos_external_execution_contract::canonical_json(&signed_assignment).unwrap();
        assert_eq!(
            observed
                .verify_import_documents(&import_bytes, &assignment_bytes)
                .unwrap()
                .authorization()
                .occurrence_id,
            "occ-1"
        );
        std::fs::write(directory.path().join("ambient-entry"), b"drift").unwrap();
        assert!(
            observed
                .verify_import_documents(&import_bytes, &assignment_bytes)
                .err()
                .unwrap()
                .to_string()
                .contains("drifted")
        );
        std::fs::remove_file(directory.path().join("ambient-entry")).unwrap();
        std::fs::write(
            &root_file,
            hex::encode(SigningKey::from_bytes(&[44; 32]).verifying_key().to_bytes()),
        )
        .unwrap();
        assert!(
            observed
                .verify_import_documents(&import_bytes, &assignment_bytes)
                .is_err()
        );
        std::fs::set_permissions(&root_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(ObservedGuestRuntime::observe(&runtime).is_err());
        std::fs::set_permissions(&root_file, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(&profile_file, br#"{"schema":1,"owner_timeout_seconds":6000,"private_source_max_bytes":33554432,"private_source_max_inodes":1024}"#).unwrap();
        assert!(ObservedGuestRuntime::observe(&runtime).is_err());
    }

    #[test]
    fn signed_ingress_requires_independent_trust_and_exact_assignment() {
        let (authorization, assignment) = fixture();
        let controller = SigningKey::from_bytes(&[41; 32]);
        let signed =
            sign_guest_import_authorization_at(authorization, &controller, &assignment, 1).unwrap();
        let verified = verify_guest_import_authorization(
            signed.clone(),
            &controller.verifying_key(),
            &assignment,
        )
        .unwrap();
        assert_eq!(
            verified.identity_digest(),
            signed.authorization.identity_digest().unwrap()
        );
        assert_eq!(verified.authorization().ticket.occurrence_id, "occ-1");

        let other = SigningKey::from_bytes(&[42; 32]);
        assert!(
            verify_guest_import_authorization(signed.clone(), &other.verifying_key(), &assignment,)
                .is_err()
        );
        assert!(verified.require_fresh_admission_at(10_000).is_err());
        let different_occurrence = GuestOccurrenceAssignment {
            occurrence_id: "occ-2",
            ..assignment
        };
        assert!(
            verify_guest_import_authorization(
                signed.clone(),
                &controller.verifying_key(),
                &different_occurrence,
            )
            .is_err()
        );

        let mut changed = signed.clone();
        changed.authorization.guest_inputs.base_snapshot.descriptor = 57;
        // Descriptor relocation preserves semantic identity, but it changes
        // the signed transfer document and must not be accepted by this key.
        assert_eq!(
            changed
                .authorization
                .guest_inputs
                .identity_digest()
                .unwrap(),
            signed.authorization.guest_inputs.identity_digest().unwrap()
        );
        assert!(
            verify_guest_import_authorization(changed, &controller.verifying_key(), &assignment,)
                .is_err()
        );
        let mut malformed = signed;
        malformed.signature_hex.push('0');
        assert!(
            verify_guest_import_authorization(malformed, &controller.verifying_key(), &assignment,)
                .is_err()
        );
    }

    #[test]
    fn document_ingress_requires_exact_assignment_key_and_canonical_bytes() {
        let (authorization, assignment) = fixture();
        let controller = SigningKey::from_bytes(&[41; 32]);
        let root = SigningKey::from_bytes(&[43; 32]);
        let signed =
            sign_guest_import_authorization_at(authorization, &controller, &assignment, 1).unwrap();
        let assignment_document = GuestOccurrenceAssignmentDocument {
            schema: GUEST_OCCURRENCE_ASSIGNMENT_SCHEMA,
            placement_thread_id: assignment.placement_thread_id.into(),
            admitted_capsule_hash: assignment.admitted_capsule_hash.into(),
            base_snapshot_hash: assignment.base_snapshot_hash.into(),
            execution_binding_hash: assignment.execution_binding_hash.into(),
            allocation_request_digest: assignment.allocation_request_digest.into(),
            occurrence_id: assignment.occurrence_id.into(),
            activation_request_digest: assignment.activation_request_digest.into(),
            supervisor_runtime_hash: assignment.supervisor_runtime_hash.into(),
            guest_runtime_manifest_hash: assignment.guest_runtime_manifest_hash.into(),
            owner_public_key_hex: hex::encode(controller.verifying_key().to_bytes()),
            attachment_deadline_ms: assignment.attachment_deadline_ms,
        };
        let signed_bytes = ryeos_external_execution_contract::canonical_json(&signed).unwrap();
        let signed_assignment =
            sign_guest_occurrence_assignment(assignment_document.clone(), &root).unwrap();
        let assignment_bytes =
            ryeos_external_execution_contract::canonical_json(&signed_assignment).unwrap();
        let verified = verify_guest_import_documents(
            &signed_bytes,
            &root.verifying_key(),
            assignment.guest_runtime_manifest_hash,
            &assignment_bytes,
        )
        .unwrap();
        assert_eq!(verified.authorization().occurrence_id, "occ-1");
        assert!(
            verify_guest_import_documents(
                &signed_bytes,
                &root.verifying_key(),
                &"9".repeat(64),
                &assignment_bytes,
            )
            .is_err(),
            "root-signed coordinates cannot substitute for installed runtime measurement"
        );

        let mut different_assignment = assignment_document.clone();
        different_assignment.guest_runtime_manifest_hash = "9".repeat(64);
        let differently_signed =
            sign_guest_occurrence_assignment(different_assignment, &root).unwrap();
        assert!(
            verify_guest_import_documents(
                &signed_bytes,
                &root.verifying_key(),
                assignment.guest_runtime_manifest_hash,
                &ryeos_external_execution_contract::canonical_json(&differently_signed).unwrap(),
            )
            .is_err()
        );
        let mut wrong_owner = assignment_document;
        wrong_owner.owner_public_key_hex =
            hex::encode(SigningKey::from_bytes(&[42; 32]).verifying_key().to_bytes());
        let wrong_owner = sign_guest_occurrence_assignment(wrong_owner, &root).unwrap();
        assert!(
            verify_guest_import_documents(
                &signed_bytes,
                &root.verifying_key(),
                assignment.guest_runtime_manifest_hash,
                &ryeos_external_execution_contract::canonical_json(&wrong_owner).unwrap(),
            )
            .is_err()
        );
        let mut altered_signature = signed_assignment;
        altered_signature.assignment.occurrence_id = "occ-other".into();
        assert!(
            verify_guest_import_documents(
                &signed_bytes,
                &root.verifying_key(),
                assignment.guest_runtime_manifest_hash,
                &ryeos_external_execution_contract::canonical_json(&altered_signature).unwrap(),
            )
            .is_err()
        );
        let mut padded = assignment_bytes.clone();
        padded.push(b'\n');
        assert!(
            verify_guest_import_documents(
                &signed_bytes,
                &root.verifying_key(),
                assignment.guest_runtime_manifest_hash,
                &padded,
            )
            .is_err()
        );
        assert!(
            verify_guest_import_documents(
                &signed_bytes,
                &SigningKey::from_bytes(&[42; 32]).verifying_key(),
                assignment.guest_runtime_manifest_hash,
                &assignment_bytes,
            )
            .is_err()
        );
    }

    #[test]
    fn expired_admission_cannot_begin_but_exact_recovery_remains_available() {
        let (authorization, assignment) = fixture();
        let controller = SigningKey::from_bytes(&[41; 32]);
        let signed =
            sign_guest_import_authorization_at(authorization, &controller, &assignment, 1).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let occurrence = lillux::PinnedDirectory::open(directory.path())
            .unwrap()
            .unwrap();
        occurrence.tighten_owner_private_directory().unwrap();
        let stale = verify_guest_import_authorization(
            signed.clone(),
            &controller.verifying_key(),
            &assignment,
        )
        .unwrap();
        assert!(
            GuestOccurrenceOwner::begin_authorized_at(&occurrence, stale, 10_000).is_err(),
            "verified token created a new owner after its admission deadline"
        );
        assert!(
            occurrence
                .open_child_directory(OsStr::new("guest-import-owner"))
                .unwrap()
                .is_none(),
            "expired authorization wrote an owner record"
        );

        let fresh = verify_guest_import_authorization(
            signed.clone(),
            &controller.verifying_key(),
            &assignment,
        )
        .unwrap();
        let owner = GuestOccurrenceOwner::begin_authorized_at(&occurrence, fresh, 1).unwrap();
        drop(owner);
        let recovery =
            verify_guest_import_authorization(signed, &controller.verifying_key(), &assignment)
                .unwrap();
        assert!(recovery.require_fresh_admission_at(10_000).is_err());
        assert!(matches!(
            recover_guest_occurrence_authorized(&occurrence, &recovery)
                .unwrap()
                .phase(),
            GuestOccurrenceRecoveryPhase::ImportUncertain
        ));
    }
}
