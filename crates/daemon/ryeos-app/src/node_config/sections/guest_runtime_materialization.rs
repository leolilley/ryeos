//! Current-node authorization to prepare a guest runtime from one exact
//! signed Bundle recipe. This is source selection, not qualification or paid
//! provider-contact authority.

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_config::{
    CompiledNodeConfigItem, NodeConfigSection, NodeConfigSourceScope, NodeItemContext,
    SectionCardinality, SectionLoadPhase, SectionLoadSpec, SectionSignerPolicy, SectionTraversal,
};

pub const SECTION_NAME: &str = "guest_runtime_materialization";
pub struct GuestRuntimeMaterializationSection;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MaterializationDocument {
    kind: String,
    schema: u32,
    protocol: String,
    recipe_ref: String,
    recipe_content_digest: String,
    recipe_effective_digest: String,
}

impl MaterializationDocument {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.kind == "node"
                && self.schema == 1
                && self.protocol == "ryeos.guest-runtime-materialization.v1",
            "unsupported guest runtime materialization binding"
        );
        let parsed = ryeos_engine::canonical_ref::CanonicalRef::parse(&self.recipe_ref)?;
        ensure!(
            parsed.to_string() == self.recipe_ref
                && parsed.kind == "config"
                && parsed.suffix.is_none()
                && lillux::valid_hash(&self.recipe_content_digest)
                && lillux::valid_hash(&self.recipe_effective_digest),
            "guest runtime materialization recipe identity is invalid"
        );
        Ok(())
    }
}

/// Only the node-signed section compiler can create an installed binding.
#[derive(Debug, Clone)]
pub struct InstalledGuestRuntimeMaterializationBinding {
    id: String,
    document: MaterializationDocument,
    digest: String,
}

impl InstalledGuestRuntimeMaterializationBinding {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }
    pub(crate) fn recipe_ref(&self) -> &str {
        &self.document.recipe_ref
    }
    pub(crate) fn require_recipe_identity(
        &self,
        recipe_ref: &str,
        raw_content_digest: &str,
        effective_digest: &str,
    ) -> Result<()> {
        ensure!(
            recipe_ref == self.document.recipe_ref
                && raw_content_digest == self.document.recipe_content_digest
                && effective_digest == self.document.recipe_effective_digest,
            "guest owner recipe differs from the exact node-signed binding"
        );
        Ok(())
    }
}

#[derive(Debug)]
struct ParsedBinding {
    id: String,
    document: MaterializationDocument,
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
            "domain": "ryeos.guest-runtime-materialization-binding.v1",
            "id": self.id,
            "signer": admission.signer_fingerprint,
            "signed_source": lillux::sha256_hex(admission.signed_source.as_bytes()),
        }))?;
        target.push_guest_runtime_materialization(InstalledGuestRuntimeMaterializationBinding {
            id: self.id,
            document: self.document,
            digest,
        })
    }
}

impl NodeConfigSection for GuestRuntimeMaterializationSection {
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
        let document: MaterializationDocument = serde_json::from_value(body.clone())
            .context("invalid guest runtime materialization binding")?;
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
    fn recipe_selection_requires_exact_content_and_effective_digests() {
        let document = MaterializationDocument {
            kind: "node".into(),
            schema: 1,
            protocol: "ryeos.guest-runtime-materialization.v1".into(),
            recipe_ref: "config:codex/guest-owner-materialization".into(),
            recipe_content_digest: "a".repeat(64),
            recipe_effective_digest: "b".repeat(64),
        };
        document.validate().unwrap();
        let mut invalid = document.clone();
        invalid.recipe_content_digest = "pending".into();
        assert!(invalid.validate().is_err());
        invalid = document.clone();
        invalid.recipe_effective_digest = "pending".into();
        assert!(invalid.validate().is_err());
        invalid = document;
        invalid.recipe_ref = "tool:codex/guest-runtime/produce".into();
        assert!(invalid.validate().is_err());

        let binding = InstalledGuestRuntimeMaterializationBinding {
            id: "owner".into(),
            document: MaterializationDocument {
                kind: "node".into(),
                schema: 1,
                protocol: "ryeos.guest-runtime-materialization.v1".into(),
                recipe_ref: "config:codex/guest-owner-materialization".into(),
                recipe_content_digest: "a".repeat(64),
                recipe_effective_digest: "b".repeat(64),
            },
            digest: "c".repeat(64),
        };
        assert!(
            binding
                .require_recipe_identity(
                    "config:codex/guest-owner-materialization",
                    &"a".repeat(64),
                    &"b".repeat(64),
                )
                .is_ok()
        );
        assert!(
            binding
                .require_recipe_identity(
                    "config:codex/guest-owner-materialization",
                    &"a".repeat(64),
                    &"d".repeat(64),
                )
                .is_err(),
            "a changed composition must not follow a pinned root recipe"
        );
    }
}
