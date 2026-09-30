//! Exact relationship among an activation receipt, its target-local content
//! binding, and one retained external runtime realization.
//!
//! This is a record join, not an authority constructor. The caller must load
//! the exact CAS records, authenticate the node-signed heads and current grant
//! where required, and independently qualify the runtime before execution.

use anyhow::{Result, ensure};

use crate::objects::{
    EXTERNAL_LARGE_CONTENT_MANIFEST_KIND, ExternalContentActivationReceipt, ExternalContentBinding,
    ExternalContentBindingState, ExternalContentConsumerAuthority, ExternalContentKind,
    ExternalContentMode, ExternalContentMountRoot, ExternalContentRealization,
    ExternalContentRealizationSet, ExternalLargeContentManifestObject,
};

/// A validated relationship only. Neither an activation receipt nor a binding
/// is a runtime-qualification claim, and this value cannot authorize a worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeContentRecordJoin {
    activation_receipt_hash: String,
    activation_program_digest: String,
    binding_hash: String,
    consumer_ref: String,
    declaration_id: String,
    realization: ExternalContentRealization,
    manifest_hash: String,
    manifest_kind: String,
    target_node_fingerprint: String,
}

impl RuntimeContentRecordJoin {
    #[allow(clippy::too_many_arguments)]
    pub fn verify(
        activation_receipt_hash: &str,
        receipt: &ExternalContentActivationReceipt,
        binding_hash: &str,
        binding: &ExternalContentBinding,
        manifest: &ExternalLargeContentManifestObject,
        realization: &ExternalContentRealization,
        expected_consumer_ref: &str,
        expected_declaration_id: &str,
        expected_manifest_hash: &str,
        expected_mount: &str,
        expected_node_fingerprint: &str,
    ) -> Result<Self> {
        receipt.validate()?;
        binding.validate()?;
        manifest.validate()?;
        ExternalContentRealizationSet::new(vec![realization.clone()])?;
        ensure!(
            crate::objects::canonical_value_digest(&receipt.to_value()?)?
                == activation_receipt_hash
                && crate::objects::canonical_value_digest(&binding.to_value()?)? == binding_hash
                && crate::objects::canonical_value_digest(&manifest.to_value()?)?
                    == binding.manifest_hash,
            "runtime content records differ from their exact CAS addresses"
        );
        let ExternalContentConsumerAuthority::InstalledBundle {
            consumer_ref,
            publisher_fingerprint,
        } = &binding.consumer
        else {
            anyhow::bail!("runtime content binding is not owned by an installed Bundle");
        };
        ensure!(
            receipt.consumer_ref == expected_consumer_ref
                && consumer_ref == expected_consumer_ref
                && receipt.publisher_fingerprint == *publisher_fingerprint
                && receipt.node_fingerprint == expected_node_fingerprint
                && binding.target_node_fingerprint == expected_node_fingerprint
                && binding.state == ExternalContentBindingState::Active
                && binding.manifest_hash == expected_manifest_hash
                && receipt
                    .components
                    .iter()
                    .any(|component| component.id == expected_declaration_id
                        && component.binding_hash == binding_hash)
                && realization.id == expected_declaration_id
                && realization.kind == ExternalContentKind::Tree
                && realization.mode == ExternalContentMode::Pinned
                && realization.mount_root == ExternalContentMountRoot::ExecutionRuntime
                && realization.mount == expected_mount
                && realization.manifest_hash == binding.manifest_hash
                && realization.entry_count == manifest.entry_count
                && realization.total_bytes > 0
                && realization.total_bytes == manifest.total_bytes
                && binding.manifest_kind == EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
            "runtime content activation, binding and realization disagree"
        );
        Ok(Self {
            activation_receipt_hash: activation_receipt_hash.to_owned(),
            activation_program_digest: receipt.activation_program_digest.clone(),
            binding_hash: binding_hash.to_owned(),
            consumer_ref: expected_consumer_ref.to_owned(),
            declaration_id: expected_declaration_id.to_owned(),
            realization: realization.clone(),
            manifest_hash: binding.manifest_hash.clone(),
            manifest_kind: binding.manifest_kind.clone(),
            target_node_fingerprint: expected_node_fingerprint.to_owned(),
        })
    }

    pub fn identity_digest(&self) -> Result<String> {
        crate::objects::canonical_value_digest(&serde_json::json!({
            "domain": "ryeos.runtime-content-record-join.v1",
            "activation_receipt_hash": self.activation_receipt_hash,
            "activation_program_digest": self.activation_program_digest,
            "binding_hash": self.binding_hash,
            "consumer_ref": self.consumer_ref,
            "declaration_id": self.declaration_id,
            "realization": self.realization,
            "manifest_hash": self.manifest_hash,
            "manifest_kind": self.manifest_kind,
            "target_node_fingerprint": self.target_node_fingerprint,
        }))
    }

    pub fn manifest_hash(&self) -> &str {
        &self.manifest_hash
    }

    /// Bind the signed probe's executable to the exact joined manifest.
    /// This is member identity, not payload or execution qualification.
    pub fn verify_recipe_member(
        &self,
        manifest: &ExternalLargeContentManifestObject,
        source: &crate::external_content::products::producer_recipe::ProducerExecutableSource,
    ) -> Result<String> {
        use crate::external_content::products::producer_recipe::ProducerExecutableSource;

        let value = manifest.to_value()?;
        ensure!(
            crate::objects::canonical_value_digest(&value)? == self.manifest_hash,
            "runtime executable manifest differs from authenticated content"
        );
        let ProducerExecutableSource::AdmittedRealizationMember {
            realization_id,
            manifest_hash,
            relative_path,
            executable_sha256,
        } = source
        else {
            anyhow::bail!("runtime probe does not select a retained realization member");
        };
        let actual = crate::external_content::runtime_member::exact_runtime_member_hash(
            &value,
            relative_path,
        )?;
        ensure!(
            realization_id == &self.declaration_id
                && manifest_hash == &self.manifest_hash
                && executable_sha256 == &actual,
            "runtime probe executable differs from authenticated content"
        );
        Ok(actual)
    }

    pub fn retained_subject(
        &self,
    ) -> Result<crate::external_content::qualification_subject::ContentQualificationSubject> {
        let subject = crate::external_content::qualification_subject::ContentQualificationSubject {
            schema:
                crate::external_content::qualification_subject::CONTENT_QUALIFICATION_SUBJECT_SCHEMA,
            activation_receipt_hash: self.activation_receipt_hash.clone(),
            activation_program_digest: self.activation_program_digest.clone(),
            binding_hash: self.binding_hash.clone(),
            consumer_ref: self.consumer_ref.clone(),
            declaration_id: self.declaration_id.clone(),
            manifest_hash: self.manifest_hash.clone(),
            manifest_kind: self.manifest_kind.clone(),
            target_node_fingerprint: self.target_node_fingerprint.clone(),
            realization: self.realization.clone(),
        };
        subject.validate()?;
        Ok(subject)
    }

    /// A deserialized coordinate must be compared to a newly authenticated
    /// record join. Matching its own hashes is never sufficient authority.
    pub fn verify_retained_subject(
        &self,
        retained: &crate::external_content::qualification_subject::ContentQualificationSubject,
    ) -> Result<()> {
        retained.validate()?;
        ensure!(
            retained == &self.retained_subject()?,
            "retained content subject differs from authenticated records"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::{
        EXTERNAL_LARGE_CONTENT_SCHEMA, ExternalContentActivationComponentReceipt,
        ExternalContentManifestEntryKind, ExternalLargeContentManifestEntry,
    };

    const CONSUMER: &str = "worker:codex/external-hosted-authoring";
    const DECLARATION: &str = "guest-runtime";

    fn fixture() -> (
        String,
        ExternalContentActivationReceipt,
        String,
        ExternalContentBinding,
        ExternalLargeContentManifestObject,
        ExternalContentRealization,
    ) {
        let publisher = "a".repeat(64);
        let node = "b".repeat(64);
        let manifest = ExternalLargeContentManifestObject {
            schema: EXTERNAL_LARGE_CONTENT_SCHEMA.into(),
            kind: EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.into(),
            entries: vec![
                ExternalLargeContentManifestEntry {
                    path: "bin".into(),
                    kind: ExternalContentManifestEntryKind::Dir,
                    mode: None,
                    blob_hash: None,
                    file_sha256: None,
                    size: None,
                    chunk_size: None,
                    chunk_hashes: Vec::new(),
                    target: None,
                },
                ExternalLargeContentManifestEntry {
                    path: "bin/codex".into(),
                    kind: ExternalContentManifestEntryKind::File,
                    mode: Some(0o755),
                    blob_hash: Some("2".repeat(64)),
                    file_sha256: None,
                    size: Some(42),
                    chunk_size: None,
                    chunk_hashes: Vec::new(),
                    target: None,
                },
            ],
            entry_count: 2,
            total_bytes: 42,
        };
        let manifest_hash =
            crate::objects::canonical_value_digest(&manifest.to_value().unwrap()).unwrap();
        let binding = ExternalContentBinding::active(
            manifest_hash.clone(),
            EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.into(),
            ExternalContentConsumerAuthority::installed_bundle(CONSUMER.into(), publisher.clone())
                .unwrap(),
            node.clone(),
            "d".repeat(64),
            "e".repeat(64),
        )
        .unwrap();
        let binding_hash =
            crate::objects::canonical_value_digest(&binding.to_value().unwrap()).unwrap();
        let receipt = ExternalContentActivationReceipt::new(
            "config:codex/guest-runtime-source-activation".into(),
            "f".repeat(64),
            CONSUMER.into(),
            publisher,
            node,
            "1".repeat(64),
            vec![ExternalContentActivationComponentReceipt {
                id: DECLARATION.into(),
                binding_hash: binding_hash.clone(),
            }],
            "d".repeat(64),
        )
        .unwrap();
        let receipt_hash =
            crate::objects::canonical_value_digest(&receipt.to_value().unwrap()).unwrap();
        let realization = ExternalContentRealization {
            id: DECLARATION.into(),
            kind: ExternalContentKind::Tree,
            mode: ExternalContentMode::Pinned,
            manifest_hash,
            entry_count: 2,
            total_bytes: 42,
            mount_root: ExternalContentMountRoot::ExecutionRuntime,
            mount: DECLARATION.into(),
        };
        (
            receipt_hash,
            receipt,
            binding_hash,
            binding,
            manifest,
            realization,
        )
    }

    #[test]
    fn exact_runtime_content_records_join_without_granting_qualification() {
        let (receipt_hash, receipt, binding_hash, binding, manifest, realization) = fixture();
        let joined = RuntimeContentRecordJoin::verify(
            &receipt_hash,
            &receipt,
            &binding_hash,
            &binding,
            &manifest,
            &realization,
            CONSUMER,
            DECLARATION,
            &binding.manifest_hash,
            DECLARATION,
            &receipt.node_fingerprint,
        )
        .unwrap();
        assert_eq!(joined.manifest_hash(), binding.manifest_hash);
        assert!(lillux::valid_hash(&joined.identity_digest().unwrap()));
        let retained = joined.retained_subject().unwrap();
        joined.verify_retained_subject(&retained).unwrap();
        let mut changed = retained;
        changed.activation_receipt_hash = "9".repeat(64);
        assert!(joined.verify_retained_subject(&changed).is_err());
    }

    #[test]
    fn runtime_member_joins_recipe_to_authenticated_manifest() {
        use crate::external_content::products::producer_recipe::ProducerExecutableSource;

        let (receipt_hash, receipt, binding_hash, binding, manifest, realization) = fixture();
        let joined = RuntimeContentRecordJoin::verify(
            &receipt_hash,
            &receipt,
            &binding_hash,
            &binding,
            &manifest,
            &realization,
            CONSUMER,
            DECLARATION,
            &binding.manifest_hash,
            DECLARATION,
            &receipt.node_fingerprint,
        )
        .unwrap();
        let source = ProducerExecutableSource::AdmittedRealizationMember {
            realization_id: DECLARATION.into(),
            manifest_hash: binding.manifest_hash.clone(),
            relative_path: "bin/codex".into(),
            executable_sha256: "2".repeat(64),
        };
        assert_eq!(
            joined.verify_recipe_member(&manifest, &source).unwrap(),
            "2".repeat(64)
        );
        for (id, hash, path, executable) in [
            (
                "other",
                binding.manifest_hash.clone(),
                "bin/codex",
                "2".repeat(64),
            ),
            (DECLARATION, "3".repeat(64), "bin/codex", "2".repeat(64)),
            (
                DECLARATION,
                binding.manifest_hash.clone(),
                "bin/missing",
                "2".repeat(64),
            ),
            (
                DECLARATION,
                binding.manifest_hash.clone(),
                "bin/codex",
                "3".repeat(64),
            ),
        ] {
            let changed = ProducerExecutableSource::AdmittedRealizationMember {
                realization_id: id.into(),
                manifest_hash: hash,
                relative_path: path.into(),
                executable_sha256: executable,
            };
            assert!(joined.verify_recipe_member(&manifest, &changed).is_err());
        }
        assert!(
            joined
                .verify_recipe_member(
                    &manifest,
                    &ProducerExecutableSource::AdmittedVerifierExecutable
                )
                .is_err()
        );
        let mut changed = manifest;
        changed.entries[1].blob_hash = Some("3".repeat(64));
        assert!(joined.verify_recipe_member(&changed, &source).is_err());
    }

    #[test]
    fn source_join_refuses_changed_consumer_node_or_realization() {
        let (receipt_hash, receipt, binding_hash, binding, manifest, realization) = fixture();
        let verify = |receipt: &ExternalContentActivationReceipt,
                      realization: &ExternalContentRealization,
                      consumer: &str,
                      manifest_hash: &str,
                      mount: &str,
                      node: &str| {
            RuntimeContentRecordJoin::verify(
                &receipt_hash,
                receipt,
                &binding_hash,
                &binding,
                &manifest,
                realization,
                consumer,
                DECLARATION,
                manifest_hash,
                mount,
                node,
            )
        };
        assert!(
            verify(
                &receipt,
                &realization,
                "worker:other/consumer",
                &binding.manifest_hash,
                DECLARATION,
                &receipt.node_fingerprint
            )
            .is_err()
        );
        assert!(
            verify(
                &receipt,
                &realization,
                CONSUMER,
                &binding.manifest_hash,
                DECLARATION,
                &"9".repeat(64)
            )
            .is_err()
        );
        assert!(
            verify(
                &receipt,
                &realization,
                CONSUMER,
                &binding.manifest_hash,
                "other-runtime",
                &receipt.node_fingerprint
            )
            .is_err()
        );
        let mut changed = realization.clone();
        changed.manifest_hash = "8".repeat(64);
        assert!(
            verify(
                &receipt,
                &changed,
                CONSUMER,
                &binding.manifest_hash,
                DECLARATION,
                &receipt.node_fingerprint
            )
            .is_err()
        );
        let mut changed = realization.clone();
        changed.kind = ExternalContentKind::File;
        assert!(
            verify(
                &receipt,
                &changed,
                CONSUMER,
                &binding.manifest_hash,
                DECLARATION,
                &receipt.node_fingerprint
            )
            .is_err()
        );
        changed.kind = ExternalContentKind::Tree;
        changed.entry_count += 1;
        assert!(
            verify(
                &receipt,
                &changed,
                CONSUMER,
                &binding.manifest_hash,
                DECLARATION,
                &receipt.node_fingerprint
            )
            .is_err()
        );
        changed.entry_count -= 1;
        changed.total_bytes += 1;
        assert!(
            verify(
                &receipt,
                &changed,
                CONSUMER,
                &binding.manifest_hash,
                DECLARATION,
                &receipt.node_fingerprint
            )
            .is_err()
        );
        assert!(
            verify(
                &receipt,
                &realization,
                CONSUMER,
                &"7".repeat(64),
                DECLARATION,
                &receipt.node_fingerprint
            )
            .is_err()
        );
    }
}
