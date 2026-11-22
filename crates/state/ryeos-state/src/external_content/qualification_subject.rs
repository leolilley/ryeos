//! Retained subject coordinate for qualification of activated external content.
//!
//! A coordinate is not a qualification claim or a content binding. Admission
//! must rejoin it to authenticated activation and binding records, then to an
//! independently settled verifier before it can support execution.
//!
//! The subject splits into two parts. The portable part names the content
//! itself from signed data alone (the activation program, consumer,
//! declaration, exact manifest and its realization) and is the same on every
//! node that activates the same signed release. The local join names the
//! records one node made of it (its signed receipt, its target-local binding
//! and the node itself). A remote content testimony is compared in portable
//! form only; the receiver supplies its own freshly authenticated local join.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::objects::{
    EXTERNAL_CONTENT_MANIFEST_KIND, EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
    ExternalContentRealization, ExternalContentRealizationSet, canonical_value_digest,
};

pub const CONTENT_QUALIFICATION_SUBJECT_SCHEMA: u32 = 1;
const PORTABLE_CONTENT_QUALIFICATION_SUBJECT_DOMAIN: &str =
    "ryeos.portable-content-qualification-subject.v1";

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

/// The node-independent identity of activated content: everything in the
/// subject that is fixed by signed data and the exact manifest. Two nodes that
/// activate the same signed release for the same consumer derive the same
/// value. It is a projection for comparison, never a persisted record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PortableContentQualificationSubject {
    pub activation_program_digest: String,
    pub consumer_ref: String,
    pub declaration_id: String,
    pub manifest_hash: String,
    pub manifest_kind: String,
    pub realization: ExternalContentRealization,
}

impl PortableContentQualificationSubject {
    /// Names of the portable fields that differ from `other`, in declaration
    /// order. Empty exactly when the two are equal.
    pub fn differing_fields(&self, other: &Self) -> Vec<&'static str> {
        let Self {
            activation_program_digest,
            consumer_ref,
            declaration_id,
            manifest_hash,
            manifest_kind,
            realization,
        } = self;
        [
            (
                "activation_program_digest",
                activation_program_digest == &other.activation_program_digest,
            ),
            ("consumer_ref", consumer_ref == &other.consumer_ref),
            ("declaration_id", declaration_id == &other.declaration_id),
            ("manifest_hash", manifest_hash == &other.manifest_hash),
            ("manifest_kind", manifest_kind == &other.manifest_kind),
            ("realization", realization == &other.realization),
        ]
        .into_iter()
        .filter_map(|(name, equal)| (!equal).then_some(name))
        .collect()
    }

    pub fn digest(&self) -> Result<String> {
        canonical_value_digest(&serde_json::json!({
            "domain": PORTABLE_CONTENT_QUALIFICATION_SUBJECT_DOMAIN,
            "subject": self,
        }))
    }
}

/// The records one node made of activated content: the receipt it signed,
/// the binding it holds for its own target, and the node itself. These differ
/// on every node and never cross between nodes as content identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentQualificationLocalJoin {
    pub activation_receipt_hash: String,
    pub binding_hash: String,
    pub target_node_fingerprint: String,
}

impl ContentQualificationLocalJoin {
    pub fn validate(&self) -> Result<()> {
        for hash in [
            &self.activation_receipt_hash,
            &self.binding_hash,
            &self.target_node_fingerprint,
        ] {
            ensure!(
                is_canonical_hash(hash),
                "content qualification local join has a noncanonical identity"
            );
        }
        Ok(())
    }
}

fn is_canonical_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl ContentQualificationSubject {
    /// The node-independent part of this subject. Written as an exhaustive
    /// destructure so a field added later cannot compile until it is placed
    /// on one side of the split.
    pub fn portable(&self) -> PortableContentQualificationSubject {
        let Self {
            schema: _,
            activation_receipt_hash: _,
            activation_program_digest,
            binding_hash: _,
            consumer_ref,
            declaration_id,
            manifest_hash,
            manifest_kind,
            target_node_fingerprint: _,
            realization,
        } = self;
        PortableContentQualificationSubject {
            activation_program_digest: activation_program_digest.clone(),
            consumer_ref: consumer_ref.clone(),
            declaration_id: declaration_id.clone(),
            manifest_hash: manifest_hash.clone(),
            manifest_kind: manifest_kind.clone(),
            realization: realization.clone(),
        }
    }

    /// The node-local part of this subject; see [`Self::portable`].
    pub fn local_join(&self) -> ContentQualificationLocalJoin {
        let Self {
            schema: _,
            activation_receipt_hash,
            activation_program_digest: _,
            binding_hash,
            consumer_ref: _,
            declaration_id: _,
            manifest_hash: _,
            manifest_kind: _,
            target_node_fingerprint,
            realization: _,
        } = self;
        ContentQualificationLocalJoin {
            activation_receipt_hash: activation_receipt_hash.clone(),
            binding_hash: binding_hash.clone(),
            target_node_fingerprint: target_node_fingerprint.clone(),
        }
    }

    /// Rejoin a portable identity to one node's local join. The split loses
    /// nothing: `from_parts(s.portable(), s.local_join()) == s`.
    pub fn from_parts(
        portable: PortableContentQualificationSubject,
        local: ContentQualificationLocalJoin,
    ) -> Result<Self> {
        let PortableContentQualificationSubject {
            activation_program_digest,
            consumer_ref,
            declaration_id,
            manifest_hash,
            manifest_kind,
            realization,
        } = portable;
        let ContentQualificationLocalJoin {
            activation_receipt_hash,
            binding_hash,
            target_node_fingerprint,
        } = local;
        let subject = Self {
            schema: CONTENT_QUALIFICATION_SUBJECT_SCHEMA,
            activation_receipt_hash,
            activation_program_digest,
            binding_hash,
            consumer_ref,
            declaration_id,
            manifest_hash,
            manifest_kind,
            target_node_fingerprint,
            realization,
        };
        subject.validate()?;
        Ok(subject)
    }

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
                is_canonical_hash(hash),
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
    fn portable_subject_ignores_only_the_local_joins() {
        let subject = subject();
        let portable = subject.portable();
        let digest = portable.digest().unwrap();
        // Another node's receipt, binding and fingerprint leave the portable
        // identity and its digest unchanged.
        let local_changes: [fn(&mut ContentQualificationSubject); 3] = [
            |s| s.activation_receipt_hash = "6".repeat(64),
            |s| s.binding_hash = "7".repeat(64),
            |s| s.target_node_fingerprint = "8".repeat(64),
        ];
        for change in local_changes {
            let mut other = subject.clone();
            change(&mut other);
            other.validate().unwrap();
            assert_ne!(other, subject);
            assert_eq!(other.portable(), portable);
            assert_eq!(other.portable().digest().unwrap(), digest);
            assert!(other.portable().differing_fields(&portable).is_empty());
            assert_ne!(other.local_join(), subject.local_join());
        }
        // Every portable field, and every member of the realization, is part
        // of the identity and is named when it differs.
        let changes: [(&str, fn(&mut ContentQualificationSubject)); 12] = [
            ("activation_program_digest", |s| {
                s.activation_program_digest = "9".repeat(64)
            }),
            ("consumer_ref", |s| {
                s.consumer_ref = "tool:fixture/other".into()
            }),
            ("declaration_id", |s| s.declaration_id = "other".into()),
            ("manifest_hash", |s| s.manifest_hash = "a".repeat(64)),
            ("manifest_kind", |s| {
                s.manifest_kind = EXTERNAL_CONTENT_MANIFEST_KIND.into()
            }),
            ("realization", |s| s.realization.id = "other".into()),
            ("realization", |s| {
                s.realization.kind = ExternalContentKind::File
            }),
            ("realization", |s| {
                s.realization.mode = ExternalContentMode::Captured
            }),
            ("realization", |s| {
                s.realization.mount_root = ExternalContentMountRoot::Project
            }),
            ("realization", |s| s.realization.mount = "elsewhere".into()),
            ("realization", |s| s.realization.entry_count += 1),
            ("realization", |s| s.realization.total_bytes += 1),
        ];
        for (field, change) in changes {
            let mut other = subject.clone();
            change(&mut other);
            let other = other.portable();
            assert_ne!(other, portable, "{field}");
            assert_ne!(other.digest().unwrap(), digest, "{field}");
            assert!(
                other.differing_fields(&portable).contains(&field),
                "{field} is not named"
            );
        }
        // The portable digest is its own domain, not the local subject digest.
        assert_ne!(digest, subject.digest().unwrap());
    }

    #[test]
    fn local_join_and_portable_rejoin_to_the_exact_subject() {
        let subject = subject();
        let join = subject.local_join();
        join.validate().unwrap();
        assert_eq!(
            ContentQualificationSubject::from_parts(subject.portable(), join.clone()).unwrap(),
            subject
        );
        let mut noncanonical = join;
        noncanonical.binding_hash = "A".repeat(64);
        assert!(noncanonical.validate().is_err());
        assert!(ContentQualificationSubject::from_parts(subject.portable(), noncanonical).is_err());
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
