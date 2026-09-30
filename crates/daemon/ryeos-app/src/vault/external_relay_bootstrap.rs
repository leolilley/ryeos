//! Protected original file-relay bootstrap. The recovered descriptor numbers
//! describe the guest's original import only; they are never host reopen paths
//! or authority to repeat the one-shot supervisor activation.

use std::ffi::OsStr;

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use lillux::crypto::Verifier as _;
use ryeos_external_execution_contract::guest_import_authorization::SignedGuestImportAuthorization;
use ryeos_state::external_execution::transport::{
    ExternalSupervisorBootstrap, MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES,
};
use zeroize::{Zeroize as _, Zeroizing};

const MASTER_KEY: &str = "INTERNAL_PLACEMENT_VAULT_RELAY_BOOTSTRAP_MASTER";
const ARTIFACT_PARENT: &str = "external-relay-bootstraps";
const MAX_SEALED_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ORIGINAL_DOCUMENT_BYTES: usize = MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES
    + ryeos_external_execution_contract::guest_import_authorization::MAX_GUEST_IMPORT_AUTHORIZATION_BYTES
    + 2048;

#[derive(serde::Serialize)]
struct OriginalRelayDocumentRef<'a> {
    schema: u32,
    bootstrap: &'a ExternalSupervisorBootstrap,
    signed_import: &'a SignedGuestImportAuthorization,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OriginalRelayDocument {
    schema: u32,
    bootstrap: ExternalSupervisorBootstrap,
    signed_import: SignedGuestImportAuthorization,
}

impl Drop for OriginalRelayDocument {
    fn drop(&mut self) {
        self.bootstrap.bootstrap_capability.zeroize();
        for value in self.bootstrap.guest_inputs.environment.values_mut() {
            value.zeroize();
        }
        for value in self
            .signed_import
            .authorization
            .guest_inputs
            .environment
            .values_mut()
        {
            value.zeroize();
        }
    }
}

/// App-owned coordinate for the original allocation request. Candidate and
/// operator paths cannot construct this type or enumerate its sealed value.
pub struct ExternalRelayBootstrapAccess {
    placement: String,
    request_digest: String,
    artifact_name: String,
}

impl ExternalRelayBootstrapAccess {
    pub(crate) fn new(placement: &str, request_digest: &str) -> Result<Self> {
        ensure!(
            !placement.is_empty() && placement.len() <= 256,
            "relay bootstrap placement is invalid"
        );
        ensure!(
            request_digest.len() == 64
                && request_digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "relay bootstrap request digest is not canonical"
        );
        Ok(Self {
            placement: placement.to_owned(),
            request_digest: request_digest.to_owned(),
            artifact_name: format!(
                "{request_digest}-{}.toml",
                lillux::sha256_hex(placement.as_bytes())
            ),
        })
    }

    pub(super) fn physical_key(&self) -> &str {
        MASTER_KEY
    }

    pub(super) fn placement(&self) -> &str {
        &self.placement
    }

    pub(super) fn request_digest(&self) -> &str {
        &self.request_digest
    }

    pub(super) fn artifact_name(&self) -> &OsStr {
        OsStr::new(&self.artifact_name)
    }

    pub(super) fn artifact_parent(&self) -> &OsStr {
        OsStr::new(ARTIFACT_PARENT)
    }

    pub(super) fn validate_bootstrap(&self, bootstrap: &ExternalSupervisorBootstrap) -> Result<()> {
        bootstrap.controller.relay()?;
        ensure!(
            bootstrap.placement_thread_id == self.placement
                && bootstrap.allocation_request_digest == self.request_digest,
            "protected relay bootstrap changed its original allocation"
        );
        ensure!(
            bootstrap.canonical_bytes()?.len() <= MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES,
            "protected relay bootstrap exceeds its exact import bound"
        );
        Ok(())
    }

    pub(super) fn canonical_document(
        &self,
        bootstrap: &ExternalSupervisorBootstrap,
        signed_import: &SignedGuestImportAuthorization,
    ) -> Result<Zeroizing<Vec<u8>>> {
        self.validate_bootstrap(bootstrap)?;
        validate_signed_import(bootstrap, signed_import)?;
        let bytes = Zeroizing::new(
            lillux::canonical_json(&serde_json::to_value(OriginalRelayDocumentRef {
                schema: 1,
                bootstrap,
                signed_import,
            })?)?
            .into_bytes(),
        );
        ensure!(
            bytes.len() <= MAX_ORIGINAL_DOCUMENT_BYTES,
            "protected original relay document exceeds its byte bound"
        );
        Ok(bytes)
    }

    pub(super) fn decode(
        &self,
        bytes: Zeroizing<Vec<u8>>,
    ) -> Result<ProtectedExternalRelayBootstrap> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_ORIGINAL_DOCUMENT_BYTES,
            "protected original relay document is outside its byte bound"
        );
        let document: OriginalRelayDocument =
            serde_json::from_slice(&bytes).context("decode protected original relay document")?;
        ensure!(document.schema == 1, "unsupported protected relay document");
        self.validate_bootstrap(&document.bootstrap)?;
        validate_signed_import(&document.bootstrap, &document.signed_import)?;
        ensure!(
            self.canonical_document(&document.bootstrap, &document.signed_import)?
                .as_slice()
                == bytes.as_slice(),
            "protected relay document changed its original canonical bytes"
        );
        Ok(ProtectedExternalRelayBootstrap(document))
    }
}

fn validate_signed_import(
    bootstrap: &ExternalSupervisorBootstrap,
    signed: &SignedGuestImportAuthorization,
) -> Result<()> {
    signed.validate_shape()?;
    let authorization = &signed.authorization;
    authorization.ticket.validate()?;
    let owner_bytes: [u8; 32] = STANDARD
        .decode(&bootstrap.owner_public_key)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("original relay owner key changed length"))?;
    let owner_key = lillux::crypto::VerifyingKey::from_bytes(&owner_bytes)?;
    let signature_bytes: [u8; 64] = hex::decode(&signed.signature_hex)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("original signed import signature changed length"))?;
    let signing_bytes = Zeroizing::new(authorization.signing_bytes()?);
    owner_key.verify(
        &signing_bytes,
        &lillux::crypto::Signature::from_bytes(&signature_bytes),
    )?;
    ensure!(
        authorization.placement_thread_id == bootstrap.placement_thread_id
            && authorization.admitted_capsule_hash == bootstrap.admitted_capsule_hash
            && authorization.base_snapshot_hash == bootstrap.base_snapshot_hash
            && authorization.execution_binding_hash == bootstrap.execution_binding_hash
            && authorization.allocation_request_digest == bootstrap.allocation_request_digest
            && authorization.occurrence_id == bootstrap.occurrence_id
            && authorization.supervisor_runtime_hash == bootstrap.supervisor_runtime_hash
            && authorization.attachment_deadline_ms == bootstrap.attachment_deadline_ms
            && authorization.guest_inputs == bootstrap.guest_inputs
            && authorization.ticket.bootstrap_sha256 == bootstrap.digest()?
            && authorization.ticket.guest_input_identity == bootstrap.guest_input_identity
            && authorization.ticket.launcher_sha256 == bootstrap.launcher_artifact_hash,
        "original signed guest import contradicts sealed relay bootstrap"
    );
    Ok(())
}

/// Kept inside the controller transport owner. No Debug, Clone or serializer.
pub struct ProtectedExternalRelayBootstrap(OriginalRelayDocument);

impl ProtectedExternalRelayBootstrap {
    pub(crate) fn bootstrap(&self) -> &ExternalSupervisorBootstrap {
        &self.0.bootstrap
    }

    pub(crate) fn signed_import(&self) -> &SignedGuestImportAuthorization {
        &self.0.signed_import
    }
}

pub(super) const MAX_ARTIFACT_BYTES: u64 = MAX_SEALED_ARTIFACT_BYTES;
