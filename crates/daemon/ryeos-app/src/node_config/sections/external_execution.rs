//! Node-owned external placement configuration. Signed configuration selects
//! authority; it is not evidence that a backend can safely execute it.

use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_config::{
    CompiledNodeConfigItem, NodeConfigSection, NodeConfigSourceScope, NodeItemContext,
    SectionCardinality, SectionLoadPhase, SectionLoadSpec, SectionSignerPolicy, SectionTraversal,
};

pub const SECTION_NAME: &str = "external_execution";
pub struct ExternalExecutionSection;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingDocument {
    kind: String,
    schema: u32,
    protocol: String,
    backend: String,
    account: String,
    credential_generation: String,
    runtime_manifest_hash: String,
    runtime_selection_identity: String,
    backend_artifact_hash: String,
    max_active: u16,
    timeout_seconds: u32,
}

impl BindingDocument {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.kind == "node" && self.schema == 1,
            "unsupported external placement binding schema"
        );
        ensure!(
            self.protocol == ryeos_state::external_execution::admission::PROTOCOL,
            "unsupported external placement protocol"
        );
        for value in [&self.backend, &self.account] {
            ensure!(
                !value.is_empty()
                    && value.len() <= 128
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')),
                "placement backend/account must be bounded identifiers, not URLs or credentials"
            );
        }
        for value in [
            &self.credential_generation,
            &self.runtime_manifest_hash,
            &self.runtime_selection_identity,
            &self.backend_artifact_hash,
        ] {
            ensure!(
                value.len() == 64
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "placement binding requires canonical content identities"
            );
        }
        ensure!(
            (1..=64).contains(&self.max_active) && (1..=3600).contains(&self.timeout_seconds),
            "placement binding limits exceed allocation bounds"
        );
        Ok(())
    }
}

fn binding_digest(id: &str, signer: &str, signed_source: &str) -> Result<String> {
    let source_hash = lillux::cas::sha256_hex(signed_source.as_bytes());
    ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "domain": "ryeos.external-placement-binding.v1", "id": id,
        "signer": signer, "signed_source": source_hash,
    }))
}

fn capacity_owner(signer: &str, document: &BindingDocument) -> Result<String> {
    ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "domain": "ryeos.external-placement-capacity.v1", "node": signer,
        "backend": document.backend, "account": document.account,
    }))
}

/// Private recovery authority retained before allocation contact. It contains
/// no credential plaintext, provider URL or occurrence token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetainedExternalExecutionBinding {
    schema: u32,
    id: String,
    document: BindingDocument,
    signed_source: String,
    signer: String,
    signer_verifying_key: [u8; 32],
    digest: String,
    capacity_owner: String,
}

impl RetainedExternalExecutionBinding {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1,
            "unsupported retained placement binding schema"
        );
        ensure!(
            !self.id.is_empty() && self.id.len() <= 128,
            "retained placement binding id is invalid"
        );
        ensure!(
            self.signer.len() == 64
                && self
                    .signer
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "retained placement signer is invalid"
        );
        ensure!(
            !self.signed_source.is_empty()
                && self.signed_source.len() <= crate::node_document::MAX_ITEM_BYTES as usize,
            "retained placement signed source is invalid"
        );
        self.document.validate()?;
        let verifying_key = lillux::crypto::VerifyingKey::from_bytes(&self.signer_verifying_key)
            .context("retained placement signer key is invalid")?;
        ensure!(
            lillux::signature::compute_fingerprint(&verifying_key) == self.signer,
            "retained placement signer key changed"
        );
        let (body, _) = lillux::signature::strip_canonical_signature_with_envelope(
            &self.signed_source,
            "#",
            None,
            false,
        )?;
        let envelope = ryeos_engine::contracts::SignatureEnvelope {
            prefix: "#".into(),
            suffix: None,
            after_shebang: false,
        };
        let header =
            ryeos_engine::item_resolution::parse_signature_header(&self.signed_source, &envelope)
                .context("retained placement source has no canonical signature")?;
        ensure!(
            header.signer_fingerprint == self.signer,
            "retained placement source signer changed"
        );
        let trust = ryeos_engine::trust::TrustStore::from_signers(vec![
            ryeos_engine::trust::TrustedSigner {
                fingerprint: self.signer.clone(),
                verifying_key,
                label: None,
            },
        ]);
        let (class, _) = ryeos_engine::trust::verify_item_signature(
            &self.signed_source,
            &header,
            &envelope,
            &trust,
        )
        .context("verify retained placement signature")?;
        ensure!(
            class == ryeos_engine::contracts::TrustClass::Trusted,
            "retained placement signature is not trusted"
        );
        let decoded: BindingDocument =
            serde_yaml::from_str(&body).context("decode retained placement signed body")?;
        ensure!(
            decoded == self.document,
            "retained placement document contradicts its signed source"
        );
        ensure!(
            self.digest == binding_digest(&self.id, &self.signer, &self.signed_source)?,
            "retained placement binding digest changed"
        );
        ensure!(
            self.capacity_owner == capacity_owner(&self.signer, &self.document)?,
            "retained placement capacity domain changed"
        );
        Ok(())
    }
    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }
    pub(crate) fn capacity_owner(&self) -> &str {
        &self.capacity_owner
    }
    pub(crate) fn canonical_json(&self) -> Result<String> {
        self.validate()?;
        Ok(lillux::canonical_json(&serde_json::to_value(self)?)?)
    }
    pub(crate) fn check_reservation_limits(
        &self,
        max_active: u16,
        timeout_seconds: u32,
    ) -> Result<()> {
        ensure!(
            max_active <= self.document.max_active
                && timeout_seconds <= self.document.timeout_seconds,
            "external reservation exceeds its retained placement limits"
        );
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn test_fixture() -> Self {
        let document = BindingDocument {
            kind: "node".into(),
            schema: 1,
            protocol: ryeos_state::external_execution::admission::PROTOCOL.into(),
            backend: "fixture".into(),
            account: "account".into(),
            credential_generation: "a".repeat(64),
            runtime_manifest_hash: "b".repeat(64),
            runtime_selection_identity: "c".repeat(64),
            backend_artifact_hash: "d".repeat(64),
            max_active: 1,
            timeout_seconds: 60,
        };
        let id = "fixture".to_owned();
        let key = lillux::crypto::SigningKey::from_bytes(&[37; 32]);
        let signer_verifying_key = key.verifying_key().to_bytes();
        let signer = lillux::signature::compute_fingerprint(&key.verifying_key());
        let body = serde_yaml::to_string(&document).unwrap();
        let signed_source =
            lillux::signature::sign_content_at(&body, &key, "#", None, "2026-09-20T00:00:00Z");
        Self {
            schema: 1,
            digest: binding_digest(&id, &signer, &signed_source).unwrap(),
            capacity_owner: capacity_owner(&signer, &document).unwrap(),
            id,
            document,
            signed_source,
            signer,
            signer_verifying_key,
        }
    }
}

/// Only verified node-config admission constructs this value. No deserialization
/// or public raw-field constructor; no credential reference in Debug output.
#[derive(Clone)]
pub struct InstalledExternalExecutionBinding {
    id: String,
    document: BindingDocument,
    signed_source: Arc<str>,
    signer: String,
    signer_verifying_key: [u8; 32],
    digest: String,
    capacity_owner: String,
}

impl std::fmt::Debug for InstalledExternalExecutionBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstalledExternalExecutionBinding")
            .field("id", &self.id)
            .field("digest", &self.digest)
            .finish_non_exhaustive()
    }
}

impl InstalledExternalExecutionBinding {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }
    pub(crate) fn capacity_owner(&self) -> &str {
        &self.capacity_owner
    }
    pub(crate) fn credential_access(
        &self,
    ) -> Result<crate::vault::placement::PlacementCredentialAccess> {
        crate::vault::placement::PlacementCredentialAccess::new(
            &self.capacity_owner,
            &self.document.credential_generation,
        )
    }
    pub(crate) fn retained_generation(&self) -> Result<RetainedExternalExecutionBinding> {
        let retained = RetainedExternalExecutionBinding {
            schema: 1,
            id: self.id.clone(),
            document: self.document.clone(),
            signed_source: self.signed_source.to_string(),
            signer: self.signer.clone(),
            signer_verifying_key: self.signer_verifying_key,
            digest: self.digest.clone(),
            capacity_owner: self.capacity_owner.clone(),
        };
        retained.validate()?;
        Ok(retained)
    }
    /// This check grants no contact permit. Backend/artifact and credential
    /// readiness must still be independently verified by the placement owner.
    pub(crate) fn check_program(
        &self,
        program: &ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram,
    ) -> Result<()> {
        program.validate()?;
        ensure!(
            program.requirement.protocol == self.document.protocol
                && program.runtime_manifest_hash == self.document.runtime_manifest_hash
                && program.selection_identity_digest == self.document.runtime_selection_identity,
            "external candidate program contradicts installed placement binding"
        );
        Ok(())
    }
}

struct ParsedBinding {
    id: String,
    document: BindingDocument,
}
impl std::fmt::Debug for ParsedBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedBinding")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl CompiledNodeConfigItem for ParsedBinding {
    fn section_name(&self) -> &'static str {
        SECTION_NAME
    }
    fn admit(
        self: Box<Self>,
        target: &mut crate::node_config::loader::NodeConfigSnapshotBuilder,
        admission: &crate::node_config::loader::NodeConfigAdmission,
    ) -> Result<()> {
        let digest = binding_digest(
            &self.id,
            &admission.signer_fingerprint,
            &admission.signed_source,
        )?;
        // Rotation changes the binding, not the account's capacity domain.
        let capacity_owner = capacity_owner(&admission.signer_fingerprint, &self.document)?;
        target.push_external_execution(InstalledExternalExecutionBinding {
            id: self.id,
            document: self.document,
            signed_source: admission.signed_source.clone(),
            signer: admission.signer_fingerprint.clone(),
            signer_verifying_key: admission.signer_verifying_key,
            digest,
            capacity_owner,
        })
    }
}

impl NodeConfigSection for ExternalExecutionSection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }
    fn source_scope(&self) -> NodeConfigSourceScope {
        NodeConfigSourceScope::AppRootOnly
    }
    fn load_spec(&self) -> SectionLoadSpec {
        SectionLoadSpec {
            phase: SectionLoadPhase::Full,
            traversal: SectionTraversal::Flat,
            signer: SectionSignerPolicy::CurrentNode,
            cardinality: SectionCardinality::Any,
        }
    }
    fn parse(
        &self,
        ctx: &NodeItemContext,
        body: &Value,
    ) -> Result<Box<dyn CompiledNodeConfigItem>> {
        let document: BindingDocument =
            serde_json::from_value(body.clone()).context("invalid external placement binding")?;
        document.validate()?;
        Ok(Box::new(ParsedBinding {
            id: ctx.id.clone(),
            document,
        }))
    }
}
