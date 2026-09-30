//! Retained subject coordinate for qualification of activated external content.
//!
//! A coordinate is not a qualification claim or a content binding. Admission
//! must rejoin it to authenticated activation and binding records, then to an
//! independently settled verifier before it can support execution.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::objects::{
    EXTERNAL_CONTENT_MANIFEST_KIND, EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
    ExternalContentRealization, ExternalContentRealizationSet, canonical_value_digest,
};

pub const CONTENT_QUALIFICATION_SUBJECT_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentQualificationSubject {
    pub schema: u32,
    pub activation_receipt_hash: String,
    pub activation_program_digest: String,
    pub binding_hash: String,
    pub consumer_ref: String,
    pub declaration_id: String,
    pub manifest_hash: String,
    pub manifest_kind: String,
    pub target_node_fingerprint: String,
    pub realization: ExternalContentRealization,
}

impl ContentQualificationSubject {
    /// Check the verifier-side slot against this exact content coordinate.
    /// The verifier slot ID can differ from the consuming declaration ID;
    /// kind, pin, manifest and metrics must still describe the same bytes.
    /// Neither this comparison nor a coordinate authorizes a verifier launch.
    pub fn verify_verifier_realizations(
        &self,
        realizations: &ExternalContentRealizationSet,
        verifier_declaration_id: &str,
    ) -> Result<()> {
        self.validate()?;
        realizations.validate()?;
        let actual = realizations
            .iter()
            .find(|entry| entry.id == verifier_declaration_id)
            .ok_or_else(|| {
                anyhow::anyhow!("verifier has no admitted qualification subject slot")
            })?;
        ensure!(
            actual.mode == crate::objects::ExternalContentMode::Pinned
                && actual.kind == self.realization.kind
                && actual.manifest_hash == self.manifest_hash
                && actual.entry_count == self.realization.entry_count
                && actual.total_bytes == self.realization.total_bytes,
            "verifier subject slot differs from activated qualification bytes"
        );
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == CONTENT_QUALIFICATION_SUBJECT_SCHEMA,
            "unsupported content qualification subject schema"
        );
        for hash in [
            &self.activation_receipt_hash,
            &self.activation_program_digest,
            &self.binding_hash,
            &self.manifest_hash,
            &self.target_node_fingerprint,
        ] {
            ensure!(
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "content qualification subject has a noncanonical identity"
            );
        }
        super::products::validate_canonical_unsuffixed_ref(
            "content qualification subject consumer",
            &self.consumer_ref,
        )?;
        ensure!(
            matches!(
                self.manifest_kind.as_str(),
                EXTERNAL_CONTENT_MANIFEST_KIND | EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
            ),
            "content qualification subject manifest kind is unsupported"
        );
        ExternalContentRealizationSet::new(vec![self.realization.clone()])?;
        ensure!(
            self.declaration_id == self.realization.id
                && self.manifest_hash == self.realization.manifest_hash,
            "content qualification subject differs from its realization"
        );
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        canonical_value_digest(&serde_json::json!({
            "domain": "ryeos.content-qualification-subject.v1",
            "subject": self,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::{ExternalContentKind, ExternalContentMode, ExternalContentMountRoot};

    fn subject() -> ContentQualificationSubject {
        ContentQualificationSubject {
            schema: CONTENT_QUALIFICATION_SUBJECT_SCHEMA,
            activation_receipt_hash: "1".repeat(64),
            activation_program_digest: "2".repeat(64),
            binding_hash: "3".repeat(64),
            consumer_ref: "worker:fixture/runtime".into(),
            declaration_id: "runtime".into(),
            manifest_hash: "4".repeat(64),
            manifest_kind: EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.into(),
            target_node_fingerprint: "5".repeat(64),
            realization: ExternalContentRealization {
                id: "runtime".into(),
                kind: ExternalContentKind::Tree,
                mode: ExternalContentMode::Pinned,
                mount_root: ExternalContentMountRoot::ExecutionRuntime,
                mount: "runtime".into(),
                manifest_hash: "4".repeat(64),
                entry_count: 2,
                total_bytes: 42,
            },
        }
    }

    #[test]
    fn verifier_slot_can_be_renamed_but_not_change_the_subject_bytes() {
        let subject = subject();
        let mut verifier = subject.realization.clone();
        verifier.id = "qualification-input".into();
        verifier.mount = "probe-input".into();
        let set = ExternalContentRealizationSet::new(vec![verifier.clone()]).unwrap();
        subject
            .verify_verifier_realizations(&set, "qualification-input")
            .unwrap();
        assert!(
            subject
                .verify_verifier_realizations(&set, "runtime")
                .is_err()
        );
        verifier.total_bytes += 1;
        let set = ExternalContentRealizationSet::new(vec![verifier]).unwrap();
        assert!(
            subject
                .verify_verifier_realizations(&set, "qualification-input")
                .is_err()
        );
    }

    #[test]
    fn subject_identity_is_exact_but_not_a_qualification_claim() {
        let subject = subject();
        subject.validate().unwrap();
        let digest = subject.digest().unwrap();
        let mut changed = subject.clone();
        changed.activation_receipt_hash = "6".repeat(64);
        assert_ne!(digest, changed.digest().unwrap());
        changed = subject.clone();
        changed.realization.mount = "other".into();
        assert_ne!(digest, changed.digest().unwrap());
        changed = subject;
        changed.manifest_hash = "7".repeat(64);
        assert!(changed.validate().is_err());
    }

    #[test]
    fn subject_accepts_canonical_consumers_without_a_kind_allowlist() {
        for consumer in [
            "worker:fixture/runtime",
            "tool:fixture/runtime",
            "custom_kind:fixture/runtime",
        ] {
            let mut subject = subject();
            subject.consumer_ref = consumer.into();
            subject.validate().unwrap();
        }
        for consumer in [
            "runtime",
            "worker:fixture/runtime@head",
            "worker:/runtime",
            "worker:fixture/../runtime",
        ] {
            let mut subject = subject();
            subject.consumer_ref = consumer.into();
            assert!(subject.validate().is_err(), "accepted {consumer}");
        }
    }
}
