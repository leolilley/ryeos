//! Controller-authorized, provider-neutral guest import intent.
//!
//! The uploaded package is not an authority source. A guest importer verifies
//! this signed document against a separately provisioned controller key and
//! protected occurrence assignment before creating its one-shot owner.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::staging_package::{GuestImportContext, GuestImportTicket};
use crate::{ExternalGuestInputProjection, canonical_json};

pub const GUEST_IMPORT_AUTHORIZATION_SCHEMA: u32 = 1;
pub const MAX_GUEST_IMPORT_AUTHORIZATION_BYTES: usize = 256 * 1024;
const SIGNATURE_DOMAIN: &[u8] = b"ryeos.external-guest-import-authorization.v1\0";

/// Independent placement coordinates held by the guest's trusted runtime.
/// Constructing this from the ticket, upload, or signed document itself would
/// make the comparison below circular and is forbidden at the ingress edge.
pub struct GuestOccurrenceAssignment<'a> {
    pub placement_thread_id: &'a str,
    pub admitted_capsule_hash: &'a str,
    pub base_snapshot_hash: &'a str,
    pub execution_binding_hash: &'a str,
    pub allocation_request_digest: &'a str,
    pub occurrence_id: &'a str,
    pub activation_request_digest: &'a str,
    pub supervisor_runtime_hash: &'a str,
    pub guest_runtime_manifest_hash: &'a str,
    pub attachment_deadline_ms: i64,
}

/// All semantic expectations needed before any package byte is imported.
/// Descriptor numbers in `guest_inputs` are transport coordinates only; the
/// eventual importer rebinds staged content under its own fixed descriptor
/// plan. The nonce distinguishes authorizations but does not by itself fence
/// replay: the occurrence owner must retain this document's digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestImportAuthorization {
    pub schema: u32,
    pub placement_thread_id: String,
    pub admitted_capsule_hash: String,
    pub base_snapshot_hash: String,
    pub execution_binding_hash: String,
    pub allocation_request_digest: String,
    pub occurrence_id: String,
    pub activation_request_digest: String,
    pub supervisor_runtime_hash: String,
    pub guest_runtime_manifest_hash: String,
    pub attachment_deadline_ms: i64,
    pub admission_deadline_ms: i64,
    pub nonce_sha256: String,
    pub ticket: GuestImportTicket,
    pub guest_inputs: ExternalGuestInputProjection,
}

/// Signature bytes are hex encoded for a strict, bounded JSON wire shape.
/// Key selection and verification belong to a guest runtime with an
/// independently pinned trust anchor, never to this document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedGuestImportAuthorization {
    pub authorization: GuestImportAuthorization,
    pub signature_hex: String,
}

fn canonical_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

impl GuestImportAuthorization {
    pub fn context(&self) -> GuestImportContext<'_> {
        GuestImportContext {
            binding_hash: &self.execution_binding_hash,
            allocation_request_digest: &self.allocation_request_digest,
            occurrence_id: &self.occurrence_id,
            activation_request_digest: &self.activation_request_digest,
        }
    }

    pub fn validate_for_assignment(
        &self,
        assignment: &GuestOccurrenceAssignment<'_>,
    ) -> Result<()> {
        ensure!(
            self.schema == GUEST_IMPORT_AUTHORIZATION_SCHEMA,
            "unsupported guest import authorization schema"
        );
        ensure!(
            !self.placement_thread_id.is_empty()
                && self.placement_thread_id.len() <= 512
                && self.placement_thread_id == assignment.placement_thread_id
                && self.occurrence_id == assignment.occurrence_id,
            "guest import authorization changed assigned placement or occurrence"
        );
        for value in [
            &self.admitted_capsule_hash,
            &self.base_snapshot_hash,
            &self.execution_binding_hash,
            &self.allocation_request_digest,
            &self.activation_request_digest,
            &self.supervisor_runtime_hash,
            &self.guest_runtime_manifest_hash,
            &self.nonce_sha256,
        ] {
            ensure!(
                canonical_digest(value),
                "guest import authorization has invalid digest"
            );
        }
        ensure!(
            self.admitted_capsule_hash == assignment.admitted_capsule_hash
                && self.base_snapshot_hash == assignment.base_snapshot_hash
                && self.execution_binding_hash == assignment.execution_binding_hash
                && self.allocation_request_digest == assignment.allocation_request_digest
                && self.activation_request_digest == assignment.activation_request_digest
                && self.supervisor_runtime_hash == assignment.supervisor_runtime_hash
                && self.guest_runtime_manifest_hash == assignment.guest_runtime_manifest_hash,
            "guest import authorization differs from protected occurrence assignment"
        );
        ensure!(
            self.attachment_deadline_ms == assignment.attachment_deadline_ms
                && self.admission_deadline_ms > 0
                && self.admission_deadline_ms <= self.attachment_deadline_ms,
            "guest import admission deadline exceeds attachment"
        );
        self.guest_inputs.validate()?;
        ensure!(
            self.guest_inputs.base_snapshot.snapshot_hash == self.base_snapshot_hash,
            "guest import base snapshot differs from protected projection"
        );
        self.ticket
            .staging_expected(&self.context(), &self.guest_inputs)?;
        ensure!(
            canonical_json(self)?.len() <= MAX_GUEST_IMPORT_AUTHORIZATION_BYTES,
            "guest import authorization exceeds byte bound"
        );
        Ok(())
    }

    /// Fresh import is forbidden after this signed deadline. Recovery may
    /// still verify the signature and exact assignment after it has elapsed.
    pub fn require_fresh_admission_at(&self, now_ms: i64) -> Result<()> {
        ensure!(
            now_ms > 0 && self.admission_deadline_ms > now_ms,
            "guest import admission deadline has elapsed"
        );
        Ok(())
    }

    pub fn signing_bytes(&self) -> Result<Vec<u8>> {
        let document = canonical_json(self)?;
        ensure!(
            document.len() <= MAX_GUEST_IMPORT_AUTHORIZATION_BYTES,
            "guest import authorization exceeds byte bound"
        );
        let mut bytes = Vec::with_capacity(SIGNATURE_DOMAIN.len() + document.len());
        bytes.extend_from_slice(SIGNATURE_DOMAIN);
        bytes.extend_from_slice(&document);
        Ok(bytes)
    }

    pub fn identity_digest(&self) -> Result<String> {
        Ok(hex::encode(Sha256::digest(self.signing_bytes()?)))
    }
}

impl SignedGuestImportAuthorization {
    pub fn validate_shape(&self) -> Result<()> {
        ensure!(
            self.signature_hex.len() == 128
                && self
                    .signature_hex
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "guest import authorization signature is not canonical hex"
        );
        ensure!(
            canonical_json(self)?.len() <= MAX_GUEST_IMPORT_AUTHORIZATION_BYTES + 256,
            "signed guest import authorization exceeds byte bound"
        );
        Ok(())
    }
}
