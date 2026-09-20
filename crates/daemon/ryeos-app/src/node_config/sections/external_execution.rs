//! Node-owned external placement configuration. Signed configuration selects
//! authority; it is not evidence that a backend can safely execute it.

use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value;

use crate::node_config::{
    CompiledNodeConfigItem, NodeConfigSection, NodeConfigSourceScope, NodeItemContext,
    SectionCardinality, SectionLoadPhase, SectionLoadSpec, SectionSignerPolicy, SectionTraversal,
};

pub const SECTION_NAME: &str = "external_execution";
pub struct ExternalExecutionSection;

#[derive(Clone, Deserialize)]
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

/// Only verified node-config admission constructs this value. No deserialization
/// or public raw-field constructor; no credential reference in Debug output.
#[derive(Clone)]
pub struct InstalledExternalExecutionBinding {
    id: String,
    document: BindingDocument,
    signed_source: Arc<str>,
    signer: String,
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
        let source_hash = lillux::cas::sha256_hex(admission.signed_source.as_bytes());
        let digest = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "domain": "ryeos.external-placement-binding.v1", "id": self.id,
            "signer": admission.signer_fingerprint, "signed_source": source_hash,
        }))?;
        // Rotation changes the binding, not the account's capacity domain.
        let capacity_owner = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "domain": "ryeos.external-placement-capacity.v1", "node": admission.signer_fingerprint,
            "backend": self.document.backend, "account": self.document.account,
        }))?;
        target.push_external_execution(InstalledExternalExecutionBinding {
            id: self.id,
            document: self.document,
            signed_source: admission.signed_source.clone(),
            signer: admission.signer_fingerprint.clone(),
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
