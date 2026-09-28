//! Current-node authority for one restored-Sandbox qualification attempt.
//!
//! The producer binding owns the provider account, credential, network scope,
//! and group. This signed section selects only the qualification profile,
//! settings, verifier artifact, and attempt budgets. It does not qualify the
//! runtime or authorize Worker allocation on its own.

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    contact_timeout_seconds: u32,
    maximum_lifetime_seconds: u32,
}

impl QualificationDocument {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.kind == "node"
                && self.schema == 1
                && self.protocol == "ryeos.runtime-snapshot-qualification.v1",
            "unsupported runtime snapshot qualification binding"
        );
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
                && (1..=3600).contains(&self.maximum_lifetime_seconds),
            "qualification contact or lifetime budget is invalid"
        );
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct InstalledRuntimeSnapshotQualificationBinding {
    id: String,
    document: QualificationDocument,
    digest: String,
}

impl InstalledRuntimeSnapshotQualificationBinding {
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
    pub(crate) fn contact_timeout_seconds(&self) -> u32 {
        self.document.contact_timeout_seconds
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
        let digest = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "domain": "ryeos.runtime-snapshot-qualification-binding.v1",
            "id": self.id,
            "signer": admission.signer_fingerprint,
            "signed_source": lillux::sha256_hex(admission.signed_source.as_bytes()),
        }))?;
        target.push_runtime_snapshot_qualification(InstalledRuntimeSnapshotQualificationBinding {
            id: self.id,
            document: self.document,
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
            schema: 1,
            protocol: "ryeos.runtime-snapshot-qualification.v1".into(),
            production_binding_id: "render-source".into(),
            production_binding_digest: "1".repeat(64),
            provider_spec_digest: "2".repeat(64),
            settings_digest: lillux::sha256_hex(
                lillux::canonical_json(&settings).unwrap().as_bytes(),
            ),
            settings,
            verifier_artifact_hash: "3".repeat(64),
            contact_timeout_seconds: 60,
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
    }
}
