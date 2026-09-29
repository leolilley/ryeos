//! Current-node authority for producing a provider snapshot from an exact
//! retained runtime product. This binding precedes, and cannot refer to, the
//! later execution binding that selects the resulting snapshot ID.

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::runtime_snapshot::MAX_RUNTIME_SNAPSHOT_UPLOAD_BYTES;
use ryeos_state::external_execution::transport::ExternalNetworkInputPolicy;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

use crate::node_config::{
    CompiledNodeConfigItem, NodeConfigSection, NodeConfigSourceScope, NodeItemContext,
    SectionCardinality, SectionLoadPhase, SectionLoadSpec, SectionSignerPolicy, SectionTraversal,
};

pub const SECTION_NAME: &str = "runtime_snapshot_production";
pub struct RuntimeSnapshotProductionSection;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProductionDocument {
    kind: String,
    schema: u32,
    protocol: String,
    backend: String,
    account: String,
    provider_group_id: String,
    credential_generation: String,
    adapter_artifact_hash: String,
    snapshot_spec_sha256: String,
    settings_digest: String,
    settings: Value,
    network_inputs: ExternalNetworkInputPolicy,
    contact_timeout_seconds: u32,
    maximum_bootstrap_lifetime_seconds: u32,
    maximum_upload_bytes: u64,
}

impl ProductionDocument {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.kind == "node"
                && self.schema == 1
                && self.protocol == "ryeos.runtime-snapshot-production.v2",
            "unsupported runtime snapshot production binding"
        );
        for (name, value) in [("backend", &self.backend), ("account", &self.account)] {
            ensure!(
                !value.is_empty()
                    && value.len() <= 128
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric()
                            || matches!(byte, b'_' | b'-' | b'.')),
                "runtime snapshot production {name} is invalid"
            );
        }
        ensure!(
            !self.provider_group_id.is_empty()
                && self.provider_group_id.len() <= 256
                && self
                    .provider_group_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')),
            "runtime snapshot production provider group is invalid"
        );
        for (name, value) in [
            ("credential generation", &self.credential_generation),
            ("adapter artifact", &self.adapter_artifact_hash),
            ("snapshot spec", &self.snapshot_spec_sha256),
            ("settings", &self.settings_digest),
        ] {
            ensure!(
                lillux::valid_hash(value),
                "runtime snapshot production {name} is invalid"
            );
        }
        let settings = lillux::canonical_json(&self.settings)?;
        ensure!(
            settings.len() <= 16 * 1024
                && self.settings.is_object()
                && lillux::sha256_hex(settings.as_bytes()) == self.settings_digest,
            "runtime snapshot production settings changed their signed digest"
        );
        self.network_inputs.validate()?;
        ensure!(
            (1..=300).contains(&self.contact_timeout_seconds)
                && (1..=3600).contains(&self.maximum_bootstrap_lifetime_seconds)
                && (16 * 1024 + 1..=MAX_RUNTIME_SNAPSHOT_UPLOAD_BYTES)
                    .contains(&self.maximum_upload_bytes),
            "runtime snapshot production contact or upload budget is invalid"
        );
        Ok(())
    }
}

/// Only the current-node-signed section compiler constructs this authority.
#[derive(Debug, Clone)]
pub struct InstalledRuntimeSnapshotProductionBinding {
    id: String,
    document: ProductionDocument,
    signer: String,
    signer_verifying_key: [u8; 32],
    signed_source: Arc<str>,
    digest: String,
}

/// Cleanup-only recovery authority retained before a source create POST.
/// It carries no credential plaintext and cannot select a new source. The
/// caller must still own an exact journaled occurrence and one-shot claim.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetainedRuntimeSnapshotProductionBinding {
    schema: u32,
    id: String,
    document: ProductionDocument,
    signed_source: String,
    signer: String,
    signer_verifying_key: [u8; 32],
    digest: String,
}

impl RetainedRuntimeSnapshotProductionBinding {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1 && !self.id.is_empty() && self.id.len() <= 128,
            "retained snapshot producer identity is invalid"
        );
        self.document.validate()?;
        let key = lillux::crypto::VerifyingKey::from_bytes(&self.signer_verifying_key)
            .context("retained snapshot producer key is invalid")?;
        ensure!(
            lillux::signature::compute_fingerprint(&key) == self.signer,
            "retained snapshot producer signer changed"
        );
        ensure!(
            !self.signed_source.is_empty()
                && self.signed_source.len() <= crate::node_document::MAX_ITEM_BYTES as usize,
            "retained snapshot producer signed source exceeds bound"
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
                .context("retained snapshot producer has no canonical signature")?;
        ensure!(
            header.signer_fingerprint == self.signer,
            "retained snapshot producer header changed signer"
        );
        let trust = ryeos_engine::trust::TrustStore::from_signers(vec![
            ryeos_engine::trust::TrustedSigner {
                fingerprint: self.signer.clone(),
                verifying_key: key,
                label: None,
            },
        ]);
        let (class, _) = ryeos_engine::trust::verify_item_signature(
            &self.signed_source,
            &header,
            &envelope,
            &trust,
        )?;
        ensure!(
            class == ryeos_engine::contracts::TrustClass::Trusted,
            "retained snapshot producer signature is invalid"
        );
        let decoded: ProductionDocument = serde_yaml::from_str(&body)?;
        ensure!(
            decoded == self.document,
            "retained snapshot producer document differs from signed source"
        );
        let digest = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "domain": "ryeos.runtime-snapshot-production-binding.v1",
            "id": self.id,
            "signer": self.signer,
            "signed_source": lillux::sha256_hex(self.signed_source.as_bytes()),
        }))?;
        ensure!(
            digest == self.digest,
            "retained snapshot producer digest changed"
        );
        Ok(())
    }

    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }

    /// Reconstructs the original signed binding. This is historical identity,
    /// not permission to make a new provider mutation: only a separate exact
    /// one-shot journal claim can authorize an unfinished stage continuation.
    /// New source and upload creation still select current node_config.
    pub(crate) fn recovered_binding(&self) -> Result<InstalledRuntimeSnapshotProductionBinding> {
        self.validate()?;
        Ok(InstalledRuntimeSnapshotProductionBinding {
            id: self.id.clone(),
            document: self.document.clone(),
            signer: self.signer.clone(),
            signer_verifying_key: self.signer_verifying_key,
            signed_source: self.signed_source.clone().into(),
            digest: self.digest.clone(),
        })
    }
}

impl InstalledRuntimeSnapshotProductionBinding {
    #[cfg(test)]
    pub(crate) fn test_fixture() -> Self {
        let settings = serde_json::json!({
            "schema": 1,
            "owner_id": "owner-fixture",
            "plan": "standard",
            "region": "oregon",
            "sandbox_group_id": "sbg-exact",
            "tls_roots_der_base64": ["fixture"],
        });
        let document = ProductionDocument {
            kind: "node".into(),
            schema: 1,
            protocol: "ryeos.runtime-snapshot-production.v2".into(),
            backend: "render-sandbox-early-access".into(),
            account: "render-test".into(),
            provider_group_id: "sbg-exact".into(),
            credential_generation: "a".repeat(64),
            adapter_artifact_hash: "b".repeat(64),
            snapshot_spec_sha256: "c".repeat(64),
            settings_digest: lillux::sha256_hex(
                lillux::canonical_json(&settings).unwrap().as_bytes(),
            ),
            settings,
            network_inputs: ExternalNetworkInputPolicy {
                resolver:
                    ryeos_state::external_execution::transport::ExternalNetworkInputSelection {
                        source: "/etc/resolv.conf".into(),
                        max_bytes: 4096,
                    },
                hosts: ryeos_state::external_execution::transport::ExternalNetworkInputSelection {
                    source: "/etc/hosts".into(),
                    max_bytes: 4096,
                },
            },
            contact_timeout_seconds: 60,
            maximum_bootstrap_lifetime_seconds: 900,
            maximum_upload_bytes: 1024 * 1024,
        };
        let key = lillux::crypto::SigningKey::from_bytes(&[37; 32]);
        let signer = lillux::signature::compute_fingerprint(&key.verifying_key());
        let signed_source = lillux::signature::sign_content_at(
            &serde_yaml::to_string(&document).unwrap(),
            &key,
            "#",
            None,
            "2026-09-29T00:00:00Z",
        );
        let id = "snapshot-producer".to_owned();
        let digest = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "domain": "ryeos.runtime-snapshot-production-binding.v1",
            "id": id, "signer": signer,
            "signed_source": lillux::sha256_hex(signed_source.as_bytes()),
        }))
        .unwrap();
        Self {
            id,
            document,
            signed_source: signed_source.into(),
            signer,
            signer_verifying_key: key.verifying_key().to_bytes(),
            digest,
        }
    }
    pub(crate) fn retained_generation(&self) -> Result<RetainedRuntimeSnapshotProductionBinding> {
        let retained = RetainedRuntimeSnapshotProductionBinding {
            schema: 1,
            id: self.id.clone(),
            document: self.document.clone(),
            signed_source: self.signed_source.to_string(),
            signer: self.signer.clone(),
            signer_verifying_key: self.signer_verifying_key,
            digest: self.digest.clone(),
        };
        retained.validate()?;
        Ok(retained)
    }
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }
    pub(crate) fn backend(&self) -> &str {
        &self.document.backend
    }
    pub(crate) fn account(&self) -> &str {
        &self.document.account
    }
    pub(crate) fn provider_group_id(&self) -> &str {
        &self.document.provider_group_id
    }
    pub(crate) fn adapter_artifact_hash(&self) -> &str {
        &self.document.adapter_artifact_hash
    }
    pub(crate) fn snapshot_spec_sha256(&self) -> &str {
        &self.document.snapshot_spec_sha256
    }
    pub(crate) fn settings(&self) -> &Value {
        &self.document.settings
    }
    pub(crate) fn settings_digest(&self) -> &str {
        &self.document.settings_digest
    }
    pub(crate) fn network_inputs(&self) -> &ExternalNetworkInputPolicy {
        &self.document.network_inputs
    }
    pub(crate) fn contact_timeout_seconds(&self) -> u32 {
        self.document.contact_timeout_seconds
    }
    pub(crate) fn maximum_bootstrap_lifetime_seconds(&self) -> u32 {
        self.document.maximum_bootstrap_lifetime_seconds
    }
    pub(crate) fn maximum_upload_bytes(&self) -> u64 {
        self.document.maximum_upload_bytes
    }

    pub(crate) fn credential_access(
        &self,
    ) -> Result<crate::vault::placement::PlacementCredentialAccess> {
        let owner = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "domain": "ryeos.runtime-snapshot-production-credential.v1",
            "signer": self.signer,
            "backend": self.document.backend,
            "account": self.document.account,
        }))?;
        crate::vault::placement::PlacementCredentialAccess::new(
            &owner,
            &self.document.credential_generation,
            &self.document.backend,
            &self.document.account,
        )
    }
}

#[derive(Debug)]
struct ParsedBinding {
    id: String,
    document: ProductionDocument,
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
        let digest = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "domain": "ryeos.runtime-snapshot-production-binding.v1",
            "id": self.id,
            "signer": admission.signer_fingerprint,
            "signed_source": lillux::sha256_hex(admission.signed_source.as_bytes()),
        }))?;
        target.push_runtime_snapshot_production(InstalledRuntimeSnapshotProductionBinding {
            id: self.id,
            document: self.document,
            signer: admission.signer_fingerprint.clone(),
            signer_verifying_key: admission.signer_verifying_key,
            signed_source: admission.signed_source.clone(),
            digest,
        })
    }
}

impl NodeConfigSection for RuntimeSnapshotProductionSection {
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
        let document: ProductionDocument = serde_json::from_value(body.clone())
            .context("invalid runtime snapshot production binding")?;
        document.validate()?;
        Ok(Box::new(ParsedBinding {
            id: ctx.id.clone(),
            document,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn producer_binding_has_exact_non_circular_authority() {
        let settings = serde_json::json!({"schema":1,"owner_id":"usr-test"});
        let document = ProductionDocument {
            kind: "node".into(),
            schema: 1,
            protocol: "ryeos.runtime-snapshot-production.v2".into(),
            backend: "render-sandbox-early-access".into(),
            account: "render-test".into(),
            provider_group_id: "sbg-test".into(),
            credential_generation: "a".repeat(64),
            adapter_artifact_hash: "b".repeat(64),
            snapshot_spec_sha256: "c".repeat(64),
            settings_digest: lillux::sha256_hex(
                lillux::canonical_json(&settings).unwrap().as_bytes(),
            ),
            settings,
            network_inputs: ExternalNetworkInputPolicy {
                resolver:
                    ryeos_state::external_execution::transport::ExternalNetworkInputSelection {
                        source: "/etc/resolv.conf".into(),
                        max_bytes: 4096,
                    },
                hosts: ryeos_state::external_execution::transport::ExternalNetworkInputSelection {
                    source: "/etc/hosts".into(),
                    max_bytes: 4096,
                },
            },
            contact_timeout_seconds: 60,
            maximum_bootstrap_lifetime_seconds: 900,
            maximum_upload_bytes: 1024 * 1024,
        };
        document.validate().unwrap();
        let mut changed = document.clone();
        changed.settings["owner_id"] = serde_json::json!("other");
        assert!(changed.validate().is_err());
        let mut changed = document.clone();
        changed.snapshot_spec_sha256 = "not-a-hash".into();
        assert!(changed.validate().is_err());
        let mut changed = document.clone();
        changed.maximum_bootstrap_lifetime_seconds = 3601;
        assert!(changed.validate().is_err());
        let key = lillux::crypto::SigningKey::from_bytes(&[37; 32]);
        let signer = lillux::signature::compute_fingerprint(&key.verifying_key());
        let signed_source = lillux::signature::sign_content_at(
            &serde_yaml::to_string(&document).unwrap(),
            &key,
            "#",
            None,
            "2026-09-29T00:00:00Z",
        );
        let id = "snapshot-producer".to_owned();
        let digest = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "domain": "ryeos.runtime-snapshot-production-binding.v1",
            "id": id,
            "signer": signer,
            "signed_source": lillux::sha256_hex(signed_source.as_bytes()),
        }))
        .unwrap();
        let installed = InstalledRuntimeSnapshotProductionBinding {
            id,
            document: document.clone(),
            signed_source: signed_source.into(),
            signer,
            signer_verifying_key: key.verifying_key().to_bytes(),
            digest,
        };
        let retained = installed.retained_generation().unwrap();
        let reopened: RetainedRuntimeSnapshotProductionBinding = serde_json::from_slice(
            &ryeos_external_execution_contract::canonical_json(&retained).unwrap(),
        )
        .unwrap();
        assert_eq!(
            reopened.recovered_binding().unwrap().digest(),
            installed.digest()
        );
        let mut tampered = reopened;
        tampered.document.provider_group_id = "sbg-other".into();
        assert!(tampered.validate().is_err());
        let mut changed = document;
        changed.contact_timeout_seconds = 301;
        assert!(changed.validate().is_err());
    }
}
