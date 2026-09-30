//! One qualification intent with explicit source provenance.
//!
//! The application derives this value from authenticated source records and
//! signed consumer allowances. Structural validation does not admit a launch
//! or authorize claims; sealed-root admission remains responsible for both.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use super::products::qualification::{
    MAX_PRODUCT_QUALIFICATION_EVIDENCE_BYTES, ProductProducerRecipeSourceIdentity,
    ProductQualificationConsumerContentIdentity, ProductQualificationConsumerDefinitionIdentity,
    ProductQualificationPolicySource, bounded, exact_coordinate, validate_claims,
};
use super::products::transfer::ProductWitnessSource;
use super::products::{validate_canonical_unsuffixed_ref, validate_hash, validate_name};
use super::qualification_execution::QualificationExecutionPurposeView;
use super::qualification_subject::ContentQualificationSubject;

pub const QUALIFICATION_LAUNCH_PURPOSE_SCHEMA: &str = "ryeos.qualification_launch_purpose.v1";

/// These sources have different authentication and CAS retention rules. An
/// activation receipt is never a producer witness or product relationship.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum QualificationSubject {
    CapturedProduct {
        product_witness_hash: String,
        witness_source: ProductWitnessSource,
        relationship_name: String,
    },
    ActivatedContent {
        content: ContentQualificationSubject,
    },
}

impl QualificationSubject {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::CapturedProduct {
                product_witness_hash,
                witness_source,
                relationship_name,
            } => {
                validate_hash("qualification product witness", product_witness_hash)?;
                witness_source.validate()?;
                validate_name(relationship_name)
            }
            Self::ActivatedContent { content } => content.validate(),
        }
    }

    /// Product owners use this checked projection before consulting product
    /// relationships or selections. No content subject can enter that lane.
    pub fn captured_product(&self) -> Result<(&str, &ProductWitnessSource, &str)> {
        match self {
            Self::CapturedProduct {
                product_witness_hash,
                witness_source,
                relationship_name,
            } => Ok((product_witness_hash, witness_source, relationship_name)),
            Self::ActivatedContent { .. } => {
                bail!("qualification subject is not a captured product")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationLaunchPurpose {
    pub schema: String,
    pub launch_id: String,
    pub owner_fingerprint: String,
    pub subject: QualificationSubject,
    pub policy_source: ProductQualificationPolicySource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer_definitions: Option<ProductQualificationConsumerDefinitionIdentity>,
    #[serde(deserialize_with = "crate::objects::deserialize_required_nullable")]
    pub consumer_content: Option<ProductQualificationConsumerContentIdentity>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub producer_recipe_sources: BTreeMap<String, ProductProducerRecipeSourceIdentity>,
    /// Verifier-side subject slot; it may differ from the consuming source's
    /// declaration ID. Its manifest must still identify the same exact bytes.
    pub subject_declaration_id: String,
    pub subject_manifest_hash: String,
    pub required_claims: Vec<String>,
    pub admitted_parameters_digest: String,
    pub verifier_ref: String,
    pub verifier_effective_definition_digest: String,
    pub verifier_realized_definition_digest: String,
}

impl QualificationLaunchPurpose {
    pub fn execution_view(&self) -> Result<QualificationExecutionPurposeView<'_>> {
        QualificationExecutionPurposeView::from_purpose(self)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema != QUALIFICATION_LAUNCH_PURPOSE_SCHEMA {
            bail!("unsupported qualification launch purpose schema");
        }
        if self.launch_id.len() != 34
            || !self.launch_id.starts_with("L-")
            || !self.launch_id[2..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("qualification launch id is not canonical");
        }
        exact_coordinate("qualification launch owner", &self.owner_fingerprint)?;
        self.subject.validate()?;
        self.policy_source.validate()?;
        match (
            &self.policy_source.policy.consumer_execution_context,
            &self.consumer_definitions,
            &self.consumer_content,
        ) {
            (Some(context), Some(definitions), Some(content)) => {
                definitions.validate_for(context)?;
                content.validate_for(context, definitions)?;
                if self.policy_source.policy.producer_scenarios.is_empty() {
                    bail!("consumer qualification has no signed direct producer scenario");
                }
            }
            (None, None, None) => {}
            _ => bail!("qualification purpose consumer definitions differ from signed policy"),
        }
        if self.producer_recipe_sources.len() != self.policy_source.policy.producer_scenarios.len()
        {
            bail!("qualification purpose does not pin every signed producer scenario");
        }
        for (name, scenario) in &self.policy_source.policy.producer_scenarios {
            let source = self
                .producer_recipe_sources
                .get(name)
                .context("qualification purpose has no producer source for signed scenario")?;
            source.validate()?;
            if source.canonical_ref != scenario.recipe_ref {
                bail!("qualification purpose producer source differs from signed scenario");
            }
            if let Some(definitions) = &self.consumer_definitions
                && source.bundle_generation_identity != definitions.bundle_generation_identity
            {
                bail!("qualification producer and consumer use different Bundle generations");
            }
        }
        validate_name(&self.subject_declaration_id)?;
        validate_hash(
            "qualification subject manifest",
            &self.subject_manifest_hash,
        )?;
        if let QualificationSubject::ActivatedContent { content } = &self.subject
            && content.manifest_hash != self.subject_manifest_hash
        {
            bail!("activated qualification subject differs from verifier manifest");
        }
        validate_claims(&self.required_claims)?;
        validate_hash(
            "qualification admitted parameters",
            &self.admitted_parameters_digest,
        )?;
        validate_canonical_unsuffixed_ref("qualification verifier", &self.verifier_ref)?;
        validate_hash(
            "qualification verifier definition",
            &self.verifier_effective_definition_digest,
        )?;
        validate_hash(
            "qualification realized verifier definition",
            &self.verifier_realized_definition_digest,
        )?;
        let policy = &self.policy_source.policy;
        if self.subject_declaration_id != policy.subject_declaration_id
            || self.verifier_ref != policy.verifier_ref
            || self.admitted_parameters_digest != policy.admitted_parameters_digest()?
            || self
                .required_claims
                .iter()
                .any(|claim| policy.allowed_claims.binary_search(claim).is_err())
        {
            bail!("qualification launch purpose contradicts its signed policy");
        }
        bounded(
            self,
            MAX_PRODUCT_QUALIFICATION_EVIDENCE_BYTES,
            "qualification launch purpose",
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::objects::{
        EXTERNAL_LARGE_CONTENT_MANIFEST_KIND, ExternalContentKind, ExternalContentMode,
        ExternalContentMountRoot, ExternalContentRealization,
    };

    pub(crate) fn content_subject(manifest_hash: &str) -> ContentQualificationSubject {
        ContentQualificationSubject {
            schema: super::super::qualification_subject::CONTENT_QUALIFICATION_SUBJECT_SCHEMA,
            activation_receipt_hash: "a".repeat(64),
            activation_program_digest: "b".repeat(64),
            binding_hash: "c".repeat(64),
            consumer_ref: "worker:fixture/runtime".into(),
            declaration_id: "guest-runtime".into(),
            manifest_hash: manifest_hash.into(),
            manifest_kind: EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.into(),
            target_node_fingerprint: "d".repeat(64),
            realization: ExternalContentRealization {
                id: "guest-runtime".into(),
                kind: ExternalContentKind::Tree,
                mode: ExternalContentMode::Pinned,
                manifest_hash: manifest_hash.into(),
                entry_count: 2,
                total_bytes: 4,
                mount_root: ExternalContentMountRoot::ExecutionRuntime,
                mount: "guest-runtime".into(),
            },
        }
    }

    #[test]
    fn explicit_content_subject_cannot_borrow_product_provenance() {
        let original = super::super::products::qualification::tests::launch_purpose();
        let mut content = original.clone();
        content.subject = QualificationSubject::ActivatedContent {
            content: content_subject(&original.subject_manifest_hash),
        };
        content.validate().unwrap();
        assert!(content.subject.captured_product().is_err());
        assert!(
            !original
                .execution_view()
                .unwrap()
                .has_same_enclosing_purpose(&content.execution_view().unwrap())
        );
        content.subject_manifest_hash = "9".repeat(64);
        assert!(content.validate().is_err());
    }

    #[test]
    fn purpose_requires_current_schema_and_explicit_source() {
        let purpose = super::super::products::qualification::tests::launch_purpose();
        let mut predecessor = purpose.clone();
        predecessor.schema = "ryeos.product_qualification_launch_purpose.v3".into();
        assert!(predecessor.validate().is_err());
        let mut missing = serde_json::to_value(&purpose).unwrap();
        missing.as_object_mut().unwrap().remove("subject");
        assert!(serde_json::from_value::<QualificationLaunchPurpose>(missing).is_err());
        let mut unknown = serde_json::to_value(&purpose).unwrap();
        unknown["subject"]["kind"] = serde_json::json!("unknown_source");
        assert!(serde_json::from_value::<QualificationLaunchPurpose>(unknown).is_err());
        let mut forged = serde_json::to_value(&purpose).unwrap();
        forged["subject"]["content"] =
            serde_json::to_value(content_subject(&purpose.subject_manifest_hash)).unwrap();
        assert!(serde_json::from_value::<QualificationLaunchPurpose>(forged).is_err());
    }
}
