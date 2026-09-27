//! Exact controller-to-guest admission before one-shot package import.
//!
//! The trusted key and occurrence assignment must come from the installed
//! guest runtime, independently of the uploaded authorization and package.

use anyhow::{Context as _, Result};
use lillux::crypto::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use ryeos_external_execution_contract::guest_import_authorization::{
    GuestImportAuthorization, GuestOccurrenceAssignment, SignedGuestImportAuthorization,
};

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

pub fn verify_guest_import_authorization(
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

    use super::*;
    use crate::guest_installation::{
        GuestOccurrenceOwner, GuestOccurrenceRecoveryPhase, recover_guest_occurrence_authorized,
    };
    use ryeos_external_execution_contract::guest_import_authorization::GUEST_IMPORT_AUTHORIZATION_SCHEMA;
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
