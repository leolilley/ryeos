//! Retained consumer-verifier source below the existing qualification purpose.
//!
//! Structural validation does not authenticate signatures, admit a launch or
//! authorize contact. The application verifies the referenced CAS bytes and
//! joins this record to its accepted root and protected occurrence authority.

use std::collections::BTreeSet;

use anyhow::{Context as _, Result, ensure};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use super::super::{validate_canonical_unsuffixed_ref, validate_hash, validate_name};
use super::{
    MAX_PRODUCT_QUALIFICATION_EVIDENCE_BYTES, ProductProducerRecipeSourceIdentity,
    ProductQualificationPolicySource, ProductQualificationProducerScenario,
    ProductQualificationRemoteVerifierSelection, bounded,
};
use crate::objects::{
    RetainedBundleExecutorSource, RetainedBundleSignerKey, RetainedSignedBundleItem,
    RetainedSignedBundleManifest,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationRemoteVerifierSource {
    pub scenario_source_digest: String,
    pub policy_resolution_blob_hash: String,
    pub recipe_resolution_blob_hash: String,
    pub producer_source: ProductProducerRecipeSourceIdentity,
    pub selection: ProductQualificationRemoteVerifierSelection,
    pub payload_bytes: u64,
    pub payload_mode: u32,
    pub signed_items: Vec<RetainedSignedBundleItem>,
    pub signed_bundle_manifests: Vec<RetainedSignedBundleManifest>,
    pub signer_keys: Vec<RetainedBundleSignerKey>,
    pub executor: RetainedBundleExecutorSource,
}

impl QualificationRemoteVerifierSource {
    pub fn validate(&self) -> Result<()> {
        validate_hash(
            "retained policy resolution",
            &self.policy_resolution_blob_hash,
        )?;
        validate_hash(
            "retained recipe resolution",
            &self.recipe_resolution_blob_hash,
        )?;
        self.producer_source.validate()?;
        self.selection.validate()?;
        let scenario = ProductQualificationProducerScenario {
            recipe_ref: self.producer_source.canonical_ref.clone(),
            remote_verifier: Some(self.selection.clone()),
        };
        ensure!(
            self.scenario_source_digest
                == scenario.remote_verifier_source_digest(&self.producer_source)?,
            "retained remote verifier source commitment changed"
        );
        ensure!(self.payload_bytes > 0 && self.payload_bytes <=
            ryeos_external_execution_contract::restored_runtime_measurement::MAX_RESTORATION_VERIFIER_BYTES,
            "retained remote verifier payload size is outside its bound");
        ensure!(
            self.payload_mode <= 0o777
                && self.payload_mode & 0o111 != 0
                && self.payload_mode & 0o022 == 0,
            "retained remote verifier payload mode is invalid"
        );
        ensure!(
            !self.signed_items.is_empty()
                && self.signed_items.len() <= 32
                && !self.signed_bundle_manifests.is_empty()
                && self.signed_bundle_manifests.len() <= 8
                && !self.signer_keys.is_empty()
                && self.signer_keys.len() <= 16,
            "retained remote verifier source inventory is outside its bounds"
        );
        ensure!(
            self.signed_items
                .windows(2)
                .all(|pair| (&pair[0].resolved_ref, &pair[0].bundle_name)
                    < (&pair[1].resolved_ref, &pair[1].bundle_name))
                && self
                    .signed_bundle_manifests
                    .windows(2)
                    .all(|pair| pair[0].bundle_name < pair[1].bundle_name)
                && self
                    .signer_keys
                    .windows(2)
                    .all(|pair| pair[0].signer_fingerprint < pair[1].signer_fingerprint),
            "retained remote verifier sources are not strictly ordered"
        );
        for item in &self.signed_items {
            validate_canonical_unsuffixed_ref(
                "retained remote verifier source item",
                &item.resolved_ref,
            )?;
            validate_name(&item.bundle_name)?;
            validate_hash("source publisher", &item.signer_fingerprint)?;
            validate_hash("signed source blob", &item.signed_blob_hash)?;
            validate_hash("source raw content", &item.raw_content_digest)?;
            item.signature_envelope.validate()?;
            ensure!(
                self.signed_bundle_manifests
                    .iter()
                    .any(|bundle| bundle.bundle_name == item.bundle_name
                        && bundle.signer_fingerprint == item.signer_fingerprint),
                "retained remote verifier source item has no Bundle manifest"
            );
        }
        for manifest in &self.signed_bundle_manifests {
            validate_name(&manifest.bundle_name)?;
            validate_hash("Bundle publisher", &manifest.signer_fingerprint)?;
            validate_hash("signed Bundle manifest", &manifest.signed_blob_hash)?;
            validate_hash("Bundle body", &manifest.body_digest)?;
            ensure!(
                self.signed_items
                    .iter()
                    .any(|item| item.bundle_name == manifest.bundle_name)
                    || manifest.bundle_name == self.executor.bundle_name,
                "retained remote verifier has an unrelated Bundle manifest"
            );
        }
        let executor = &self.executor;
        let bundle_name = self
            .selection
            .binary_ref
            .strip_prefix("bin:")
            .and_then(|path| path.split_once('/'))
            .map(|(bundle, _)| bundle)
            .context("retained remote verifier has no Bundle selection")?;
        ensure!(
            executor.bundle_name == bundle_name
                && executor.item_ref == self.selection.bundle_payload_ref()?
                && executor.target_triple == self.selection.guest_target_triple
                && executor.signer_fingerprint == self.producer_source.publisher_fingerprint,
            "retained remote verifier executor differs from signed selection"
        );
        ensure!(
            self.signed_bundle_manifests
                .iter()
                .any(|manifest| manifest.bundle_name == executor.bundle_name
                    && manifest.signer_fingerprint == executor.signer_fingerprint),
            "retained remote verifier executor has no matching signed Bundle"
        );
        for (label, hash) in [
            ("executor payload", &executor.payload_blob_hash),
            (
                "signed executor ref",
                &executor.signed_manifest_ref_blob_hash,
            ),
            ("executor manifest", &executor.manifest_object_blob_hash),
            ("executor ItemSource", &executor.item_source_object_hash),
            (
                "signed executor sidecar",
                &executor.signed_sidecar_blob_hash,
            ),
        ] {
            validate_hash(label, hash)?;
        }
        let referenced: BTreeSet<_> = self
            .signed_items
            .iter()
            .map(|item| item.signer_fingerprint.as_str())
            .chain(
                self.signed_bundle_manifests
                    .iter()
                    .map(|manifest| manifest.signer_fingerprint.as_str()),
            )
            .chain(std::iter::once(executor.signer_fingerprint.as_str()))
            .collect();
        ensure!(
            referenced
                == self
                    .signer_keys
                    .iter()
                    .map(|key| key.signer_fingerprint.as_str())
                    .collect::<BTreeSet<_>>(),
            "retained remote verifier keys differ from referenced publishers"
        );
        for key in &self.signer_keys {
            let encoded = key
                .verifying_key
                .strip_prefix("ed25519:")
                .context("retained remote verifier public key is not Ed25519")?;
            let bytes: [u8; 32] = base64::engine::general_purpose::STANDARD
                .decode(encoded)?
                .try_into()
                .map_err(|_| {
                    anyhow::anyhow!("retained remote verifier public key has wrong length")
                })?;
            let verifier = lillux::crypto::VerifyingKey::from_bytes(&bytes)?;
            ensure!(
                !verifier.is_weak()
                    && lillux::crypto::fingerprint(&verifier) == key.signer_fingerprint
                    && base64::engine::general_purpose::STANDARD.encode(bytes) == encoded,
                "retained remote verifier public key differs from publisher"
            );
        }
        ensure!(
            self.signed_items.iter().any(|item| item.resolved_ref
                == self.producer_source.canonical_ref
                && item.raw_content_digest == self.producer_source.raw_content_digest
                && item.signer_fingerprint == self.producer_source.publisher_fingerprint),
            "retained remote verifier has no exact signed recipe source"
        );
        bounded(
            self,
            MAX_PRODUCT_QUALIFICATION_EVIDENCE_BYTES,
            "retained remote verifier source",
        )
    }

    pub fn validate_for(
        &self,
        policy: &ProductQualificationPolicySource,
        scenario_id: &str,
    ) -> Result<()> {
        self.validate()?;
        policy.validate()?;
        let scenario = policy
            .policy
            .producer_scenarios
            .get(scenario_id)
            .context("retained remote verifier scenario is absent from signed policy")?;
        ensure!(
            scenario.recipe_ref == self.producer_source.canonical_ref
                && scenario.remote_verifier.as_ref() == Some(&self.selection),
            "retained remote verifier differs from signed policy scenario"
        );
        ensure!(
            self.signed_items
                .iter()
                .any(|item| item.resolved_ref == policy.canonical_ref
                    && item.raw_content_digest == policy.raw_content_digest
                    && item.signer_fingerprint == policy.publisher_fingerprint),
            "retained remote verifier has no exact signed policy source"
        );
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::objects::RetainedSignatureEnvelope;

    pub(crate) fn fixture() -> (
        QualificationRemoteVerifierSource,
        ProductQualificationPolicySource,
    ) {
        let key = lillux::crypto::SigningKey::from_bytes(&[17u8; 32]).verifying_key();
        let publisher = lillux::crypto::fingerprint(&key);
        let mut policy = super::super::tests::launch_purpose().policy_source;
        policy.publisher_fingerprint = publisher.clone();
        let selection = ProductQualificationRemoteVerifierSelection {
            binary_ref: "bin:fixtures/consumer-verifier".into(),
            guest_target_triple: "x86_64-unknown-linux-musl".into(),
        };
        let producer_source = ProductProducerRecipeSourceIdentity {
            bundle_generation_identity: "generation-1".into(),
            canonical_ref: "config:fixtures/recipe".into(),
            raw_content_digest: "a".repeat(64),
            effective_definition_digest: "b".repeat(64),
            publisher_fingerprint: publisher.clone(),
            recipe_digest: "c".repeat(64),
        };
        let scenario = ProductQualificationProducerScenario {
            recipe_ref: producer_source.canonical_ref.clone(),
            remote_verifier: Some(selection.clone()),
        };
        let source = QualificationRemoteVerifierSource {
            policy_resolution_blob_hash: "1".repeat(64),
            recipe_resolution_blob_hash: "2".repeat(64),
            scenario_source_digest: scenario
                .remote_verifier_source_digest(&producer_source)
                .unwrap(),
            selection,
            producer_source: producer_source.clone(),
            payload_bytes: 1024,
            payload_mode: 0o755,
            signed_items: vec![
                RetainedSignedBundleItem {
                    resolved_ref: policy.canonical_ref.clone(),
                    bundle_name: "fixtures".into(),
                    signer_fingerprint: publisher.clone(),
                    signed_blob_hash: "d".repeat(64),
                    raw_content_digest: policy.raw_content_digest.clone(),
                    signature_envelope: RetainedSignatureEnvelope {
                        prefix: "#".into(),
                        suffix: None,
                        after_shebang: false,
                    },
                },
                RetainedSignedBundleItem {
                    resolved_ref: producer_source.canonical_ref.clone(),
                    bundle_name: "fixtures".into(),
                    signer_fingerprint: publisher.clone(),
                    signed_blob_hash: "e".repeat(64),
                    raw_content_digest: producer_source.raw_content_digest.clone(),
                    signature_envelope: RetainedSignatureEnvelope {
                        prefix: "#".into(),
                        suffix: None,
                        after_shebang: false,
                    },
                },
            ],
            signed_bundle_manifests: vec![RetainedSignedBundleManifest {
                bundle_name: "fixtures".into(),
                signer_fingerprint: publisher.clone(),
                signed_blob_hash: "f".repeat(64),
                body_digest: "1".repeat(64),
            }],
            signer_keys: vec![RetainedBundleSignerKey {
                signer_fingerprint: publisher.clone(),
                verifying_key: format!(
                    "ed25519:{}",
                    base64::engine::general_purpose::STANDARD.encode(key.to_bytes())
                ),
            }],
            executor: RetainedBundleExecutorSource {
                bundle_name: "fixtures".into(),
                item_ref: "bin/x86_64-unknown-linux-musl/consumer-verifier".into(),
                target_triple: "x86_64-unknown-linux-musl".into(),
                signer_fingerprint: publisher,
                signed_manifest_ref_blob_hash: "2".repeat(64),
                manifest_object_blob_hash: "3".repeat(64),
                item_source_object_hash: "4".repeat(64),
                signed_sidecar_blob_hash: "5".repeat(64),
                payload_blob_hash: "6".repeat(64),
            },
        };
        policy
            .policy
            .producer_scenarios
            .insert("remote_codex".into(), scenario);
        (source, policy)
    }

    #[test]
    fn source_identity_requires_exact_selection_and_complete_provenance() {
        let (source, policy) = fixture();
        source.validate_for(&policy, "remote_codex").unwrap();
        let mut changed = source.clone();
        changed.executor.item_ref = "bin/x86_64-unknown-linux-musl/codex".into();
        assert!(changed.validate().is_err());
        changed = source.clone();
        changed.payload_mode = 0o777;
        assert!(changed.validate().is_err());
        changed = source.clone();
        changed.policy_resolution_blob_hash.clear();
        assert!(changed.validate().is_err());
        changed = source.clone();
        changed.recipe_resolution_blob_hash.clear();
        assert!(changed.validate().is_err());
        changed = source.clone();
        changed.signed_items[0].signer_fingerprint = "a".repeat(64);
        assert!(changed.validate().is_err());
        changed = source.clone();
        changed.signer_keys.clear();
        assert!(changed.validate().is_err());
        changed = source.clone();
        changed.signed_items.reverse();
        assert!(changed.validate().is_err());
        changed = source.clone();
        changed.signed_items.remove(0);
        assert!(changed.validate_for(&policy, "remote_codex").is_err());
        let mut changed_policy = policy.clone();
        changed_policy
            .policy
            .producer_scenarios
            .get_mut("remote_codex")
            .unwrap()
            .remote_verifier = None;
        assert!(
            source
                .validate_for(&changed_policy, "remote_codex")
                .is_err()
        );
    }
}
