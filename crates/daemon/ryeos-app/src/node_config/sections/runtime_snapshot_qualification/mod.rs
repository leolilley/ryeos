//! Current-node authority for one restored-Sandbox qualification attempt.
//!
//! The producer binding owns the provider account, credential, network scope,
//! and group. This signed section selects only the qualification profile,
//! settings, verifier artifact, and attempt budgets. It does not qualify the
//! runtime or authorize Worker allocation on its own.

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::restored_runtime_measurement::{
    ConsumerRuntimeVerificationCoordinate, ConsumerRuntimeVerifierSelection,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

pub mod compose;

use crate::node_config::{
    CompiledNodeConfigItem, NodeConfigSection, NodeConfigSourceScope, NodeItemContext,
    SectionCardinality, SectionLoadPhase, SectionLoadSpec, SectionSignerPolicy, SectionTraversal,
};

pub const SECTION_NAME: &str = "runtime_snapshot_qualification";
pub struct RuntimeSnapshotQualificationSection;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QualificationDocument {
    kind: String,
    schema: u32,
    protocol: String,
    production_binding_id: String,
    production_binding_digest: String,
    provider_spec_digest: String,
    settings_digest: String,
    settings: Value,
    verifier_artifact_hash: String,
    consumer_verifiers: BTreeMap<String, ConsumerRuntimeVerifierSelection>,
    contact_timeout_seconds: u32,
    /// Budget of the one contact that delivers a consumer verifier archive
    /// into the restored occurrence. An archive transfer outlasts any plain
    /// provider round trip, exactly as a Q guest package outlasts its contact.
    delivery_timeout_seconds: u32,
    maximum_lifetime_seconds: u32,
}

impl QualificationDocument {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.kind == "node"
                && self.schema == 4
                && self.protocol == "ryeos.runtime-snapshot-qualification.v4",
            "unsupported runtime snapshot qualification binding"
        );
        ensure!(
            self.consumer_verifiers.len() <= 8,
            "snapshot qualification consumer verifier inventory exceeds bound"
        );
        for (scenario, selection) in &self.consumer_verifiers {
            ensure!(
                !scenario.is_empty()
                    && scenario.len() <= 128
                    && scenario
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
                "consumer verifier scenario is invalid"
            );
            selection.validate()?;
        }
        ensure!(
            !self.production_binding_id.is_empty()
                && self.production_binding_id.len() <= 256
                && self.production_binding_id.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'/')
                }),
            "qualification production binding ID is invalid"
        );
        for (name, value) in [
            ("production binding", &self.production_binding_digest),
            ("provider spec", &self.provider_spec_digest),
            ("settings", &self.settings_digest),
            ("verifier artifact", &self.verifier_artifact_hash),
        ] {
            ensure!(
                lillux::valid_hash(value),
                "qualification {name} digest is invalid"
            );
        }
        let settings = lillux::canonical_json(&self.settings)?;
        ensure!(
            self.settings.is_object()
                && settings.len() <= 16 * 1024
                && lillux::sha256_hex(settings.as_bytes()) == self.settings_digest,
            "qualification settings changed their signed digest"
        );
        ensure!(
            (1..=300).contains(&self.contact_timeout_seconds)
                && (1..=3600).contains(&self.maximum_lifetime_seconds)
                && (self.contact_timeout_seconds..=self.maximum_lifetime_seconds)
                    .contains(&self.delivery_timeout_seconds),
            "qualification contact, delivery or lifetime budget is invalid"
        );
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct InstalledRuntimeSnapshotQualificationBinding {
    id: String,
    document: QualificationDocument,
    signer: String,
    signer_verifying_key: [u8; 32],
    signed_source: Arc<str>,
    digest: String,
}

fn binding_digest(id: &str, signer: &str, signed_source: &str) -> Result<String> {
    ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "domain": "ryeos.runtime-snapshot-qualification-binding.v1",
        "id": id,
        "signer": signer,
        "signed_source": lillux::sha256_hex(signed_source.as_bytes()),
    }))
}

/// The signed qualification generation an open qualification occurrence was
/// created under, retained with the occurrence so its termination does not
/// depend on what node config pins now. It carries no credential and cannot
/// create a new occurrence: only the occurrence's own termination reads it.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetainedRuntimeSnapshotQualificationBinding {
    schema: u32,
    id: String,
    document: QualificationDocument,
    signed_source: String,
    signer: String,
    signer_verifying_key: [u8; 32],
    digest: String,
}

impl RetainedRuntimeSnapshotQualificationBinding {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1 && !self.id.is_empty() && self.id.len() <= 128,
            "retained snapshot qualification identity is invalid"
        );
        self.document.validate()?;
        let key = lillux::crypto::VerifyingKey::from_bytes(&self.signer_verifying_key)
            .context("retained snapshot qualification key is invalid")?;
        ensure!(
            lillux::signature::compute_fingerprint(&key) == self.signer,
            "retained snapshot qualification signer changed"
        );
        ensure!(
            !self.signed_source.is_empty()
                && self.signed_source.len()
                    <= ryeos_node_runtime::node_document::MAX_ITEM_BYTES as usize,
            "retained snapshot qualification signed source exceeds bound"
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
                .context("retained snapshot qualification has no canonical signature")?;
        ensure!(
            header.signer_fingerprint == self.signer,
            "retained snapshot qualification header changed signer"
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
            &ryeos_engine::trust::SignerContext::node("node_config"),
        )?;
        ensure!(
            class == ryeos_engine::contracts::TrustClass::Trusted,
            "retained snapshot qualification signature is invalid"
        );
        let decoded: QualificationDocument = serde_yaml::from_str(&body)?;
        ensure!(
            decoded == self.document,
            "retained snapshot qualification document differs from signed source"
        );
        ensure!(
            binding_digest(&self.id, &self.signer, &self.signed_source)? == self.digest,
            "retained snapshot qualification digest changed"
        );
        Ok(())
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }

    /// Reconstructs the signed binding the occurrence was created under.
    /// Historical identity for that occurrence's termination only; a new
    /// occurrence still selects current node config.
    pub(crate) fn recovered_binding(&self) -> Result<InstalledRuntimeSnapshotQualificationBinding> {
        self.validate()?;
        Ok(InstalledRuntimeSnapshotQualificationBinding {
            id: self.id.clone(),
            document: self.document.clone(),
            signer: self.signer.clone(),
            signer_verifying_key: self.signer_verifying_key,
            signed_source: self.signed_source.clone().into(),
            digest: self.digest.clone(),
        })
    }
}

impl InstalledRuntimeSnapshotQualificationBinding {
    /// A qualification binding that names the given producer and pins the
    /// given verifier and provider spec, for tests that compare pins with
    /// installed artifacts. Its own digest is a fixed stand-in, not a signed
    /// source digest.
    #[cfg(test)]
    pub(crate) fn test_fixture(
        production_binding_id: &str,
        production_binding_digest: &str,
        provider_spec_digest: &str,
        verifier_artifact_hash: &str,
        consumer_verifiers: BTreeMap<String, ConsumerRuntimeVerifierSelection>,
    ) -> Self {
        let settings = serde_json::json!({"schema":1,"owner_id":"owner-1"});
        let document = QualificationDocument {
            kind: "node".into(),
            schema: 4,
            protocol: "ryeos.runtime-snapshot-qualification.v4".into(),
            production_binding_id: production_binding_id.to_owned(),
            production_binding_digest: production_binding_digest.to_owned(),
            provider_spec_digest: provider_spec_digest.to_owned(),
            settings_digest: lillux::sha256_hex(
                lillux::canonical_json(&settings).unwrap().as_bytes(),
            ),
            settings,
            verifier_artifact_hash: verifier_artifact_hash.to_owned(),
            consumer_verifiers,
            contact_timeout_seconds: 60,
            delivery_timeout_seconds: 600,
            maximum_lifetime_seconds: 900,
        };
        document.validate().unwrap();
        Self {
            id: "snapshot-qualification".into(),
            document,
            signer: "0".repeat(64),
            signer_verifying_key: [0; 32],
            signed_source: Arc::from(""),
            digest: "9".repeat(64),
        }
    }

    /// A node-signed qualification binding whose digest is computed from its
    /// signed source, so it can be retained and recovered.
    #[cfg(test)]
    pub(crate) fn signed_test_fixture(
        production_binding_id: &str,
        production_binding_digest: &str,
        provider_spec_digest: &str,
        verifier_artifact_hash: &str,
    ) -> Self {
        let mut fixture = Self::test_fixture(
            production_binding_id,
            production_binding_digest,
            provider_spec_digest,
            verifier_artifact_hash,
            BTreeMap::new(),
        );
        let key = lillux::crypto::SigningKey::from_bytes(&[37; 32]);
        fixture.signer = lillux::signature::compute_fingerprint(&key.verifying_key());
        fixture.signer_verifying_key = key.verifying_key().to_bytes();
        let signed = lillux::signature::sign_content_at(
            &serde_yaml::to_string(&fixture.document).unwrap(),
            &key,
            "#",
            None,
            "2026-11-14T00:00:00Z",
        );
        fixture.digest = binding_digest(&fixture.id, &fixture.signer, &signed).unwrap();
        fixture.signed_source = signed.into();
        fixture
    }

    pub(crate) fn retained_generation(
        &self,
    ) -> Result<RetainedRuntimeSnapshotQualificationBinding> {
        let retained = RetainedRuntimeSnapshotQualificationBinding {
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

    pub(crate) fn consumer_verifier(
        &self,
        coordinate: &ConsumerRuntimeVerificationCoordinate,
    ) -> Result<&ConsumerRuntimeVerifierSelection> {
        coordinate.validate()?;
        self.consumer_verifier_for_source(
            &coordinate.scenario_id,
            &coordinate.scenario_source_digest,
        )
    }

    /// Exact static source selection before a verifier root exists.
    pub(crate) fn consumer_verifier_for_source(
        &self,
        scenario_id: &str,
        scenario_source_digest: &str,
    ) -> Result<&ConsumerRuntimeVerifierSelection> {
        let selection = self
            .document
            .consumer_verifiers
            .get(scenario_id)
            .context("signed qualification profile has no selected consumer verifier")?;
        selection.validate()?;
        ensure!(
            selection.scenario_source_digest == scenario_source_digest,
            "signed qualification profile selected a different consumer source"
        );
        Ok(selection)
    }
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }
    pub(crate) fn production_binding_id(&self) -> &str {
        &self.document.production_binding_id
    }
    pub(crate) fn production_binding_digest(&self) -> &str {
        &self.document.production_binding_digest
    }
    pub(crate) fn provider_spec_digest(&self) -> &str {
        &self.document.provider_spec_digest
    }
    pub(crate) fn settings_digest(&self) -> &str {
        &self.document.settings_digest
    }
    pub(crate) fn settings(&self) -> &Value {
        &self.document.settings
    }
    pub(crate) fn verifier_artifact_hash(&self) -> &str {
        &self.document.verifier_artifact_hash
    }
    pub(crate) fn consumer_verifiers(&self) -> &BTreeMap<String, ConsumerRuntimeVerifierSelection> {
        &self.document.consumer_verifiers
    }
    pub(crate) fn contact_timeout_seconds(&self) -> u32 {
        self.document.contact_timeout_seconds
    }
    pub(crate) fn delivery_timeout_seconds(&self) -> u32 {
        self.document.delivery_timeout_seconds
    }
    pub(crate) fn maximum_lifetime_seconds(&self) -> u32 {
        self.document.maximum_lifetime_seconds
    }
}

#[derive(Debug)]
struct ParsedBinding {
    id: String,
    document: QualificationDocument,
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
        target.push_runtime_snapshot_qualification(InstalledRuntimeSnapshotQualificationBinding {
            id: self.id,
            document: self.document,
            signer: admission.signer_fingerprint.clone(),
            signer_verifying_key: admission.signer_verifying_key,
            signed_source: admission.signed_source.clone(),
            digest,
        })
    }
}

impl NodeConfigSection for RuntimeSnapshotQualificationSection {
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
        crate::node_config::coherence::require_admitted_binding_schema(
            SECTION_NAME,
            body,
            &[4],
            "ryeos.runtime-snapshot-qualification.v4",
        )?;
        let document: QualificationDocument = serde_json::from_value(body.clone())
            .context("invalid runtime snapshot qualification binding")?;
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

    fn document() -> QualificationDocument {
        let settings = serde_json::json!({"schema":1,"owner_id":"owner-1"});
        QualificationDocument {
            kind: "node".into(),
            schema: 4,
            protocol: "ryeos.runtime-snapshot-qualification.v4".into(),
            production_binding_id: "test-source".into(),
            production_binding_digest: "1".repeat(64),
            provider_spec_digest: "2".repeat(64),
            settings_digest: lillux::sha256_hex(
                lillux::canonical_json(&settings).unwrap().as_bytes(),
            ),
            settings,
            verifier_artifact_hash: "3".repeat(64),
            consumer_verifiers: BTreeMap::new(),
            contact_timeout_seconds: 60,
            delivery_timeout_seconds: 600,
            maximum_lifetime_seconds: 900,
        }
    }

    #[test]
    fn signed_qualification_binding_refuses_ambient_or_changed_settings() {
        document().validate().unwrap();
        let mut changed = document();
        changed.settings["owner_id"] = serde_json::json!("other-owner");
        assert!(changed.validate().is_err());
        changed = document();
        changed.production_binding_id = "../ambient".into();
        assert!(changed.validate().is_err());
        changed = document();
        changed.maximum_lifetime_seconds = 3601;
        assert!(changed.validate().is_err());
        // The delivery budget sits between one contact and the whole lifetime.
        changed = document();
        changed.delivery_timeout_seconds = 59;
        assert!(changed.validate().is_err());
        changed = document();
        changed.delivery_timeout_seconds = 901;
        assert!(changed.validate().is_err());
        changed = document();
        changed.delivery_timeout_seconds = 900;
        changed.validate().unwrap();
    }

    #[test]
    fn consumer_selection_is_explicit_bounded_and_distinct_from_owner_artifact() {
        let mut predecessor = document();
        predecessor.schema = 3;
        predecessor.protocol = "ryeos.runtime-snapshot-qualification.v3".into();
        assert!(predecessor.validate().is_err());
        let mut selected = document();
        selected.consumer_verifiers.insert(
            "routed-consumer".into(),
            ConsumerRuntimeVerifierSelection {
                scenario_source_digest: "4".repeat(64),
                verifier_artifact_hash: "5".repeat(64),
                archive_budget: ryeos_external_execution_contract::restored_runtime_measurement::ConsumerArchiveBudget::new(16, 4096, 8192).unwrap(),
            },
        );
        selected.validate().unwrap();
        assert_ne!(
            selected.consumer_verifiers["routed-consumer"].verifier_artifact_hash,
            selected.verifier_artifact_hash
        );
        selected
            .consumer_verifiers
            .get_mut("routed-consumer")
            .unwrap()
            .verifier_artifact_hash
            .clear();
        assert!(selected.validate().is_err());
        let mut oversized = document();
        for index in 0..9 {
            oversized.consumer_verifiers.insert(
                format!("scenario-{index}"),
                ConsumerRuntimeVerifierSelection {
                    scenario_source_digest: "4".repeat(64),
                    verifier_artifact_hash: "5".repeat(64),
                    archive_budget: ryeos_external_execution_contract::restored_runtime_measurement::ConsumerArchiveBudget::new(16, 4096, 8192).unwrap(),
                },
            );
        }
        assert!(oversized.validate().is_err());
        let mut absent = serde_json::to_value(document()).unwrap();
        absent.as_object_mut().unwrap().remove("consumer_verifiers");
        assert!(serde_json::from_value::<QualificationDocument>(absent).is_err());
    }
}
