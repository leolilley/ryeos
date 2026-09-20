use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicBool, Ordering},
};

use ryeos_bundle_publication_contract::{
    BUNDLE_GENERATION_KIND, BUNDLE_GENERATION_SCHEMA, BundleBuildProfile, BundleGeneration,
    BundleTarget, PUBLISHER_MATERIALIZATION_RESULT_KIND, PUBLISHER_MATERIALIZATION_RESULT_SCHEMA,
    PublisherMaterializationResult, PublisherMutationContract,
};
use ryeos_state::{
    external_content::products::accepted_result::{
        PRODUCT_BUILD_ACCEPTED_RESULT_KIND, PRODUCT_BUILD_ACCEPTED_RESULT_SCHEMA,
    },
    objects::{EXTERNAL_CONTENT_MANIFEST_KIND, EXTERNAL_CONTENT_TREE_SCHEMA},
};
use serde_json::{Value, json};

use super::*;

#[derive(Default)]
struct MemoryObjects(BTreeMap<String, Value>);

impl MemoryObjects {
    fn insert(&mut self, value: Value) -> String {
        let hash = lillux::cas::sha256_hex(lillux::canonical_json(&value).unwrap().as_bytes());
        self.0.insert(hash.clone(), value);
        hash
    }
}

impl PublicationObjectReader for MemoryObjects {
    fn get_object(&self, hash: &str) -> anyhow::Result<Option<Value>> {
        Ok(self.0.get(hash).cloned())
    }
}

struct Proof {
    called: AtomicBool,
    accept: bool,
}

impl PublisherMaterializationProof for Proof {
    fn verify_closed_mutation(
        &self,
        result: &PublisherMaterializationResult,
        input: &ExternalContentManifestObject,
        output: &ExternalContentManifestObject,
    ) -> anyhow::Result<()> {
        self.called.store(true, Ordering::Relaxed);
        assert_eq!(
            result.mutation_contract,
            PublisherMutationContract::RyeosBundleSignV1
        );
        assert_ne!(input, output);
        if !self.accept {
            anyhow::bail!("fixture rejects transformation");
        }
        Ok(())
    }
}

impl BundleReleaseEvidenceProof for Proof {
    fn verify_release_evidence(
        &self,
        _generation: &BundleGeneration,
        _accepted_result: &ProductBuildAcceptedResult,
        _accepted_capture_result: &ProductBuildAcceptedResult,
        _materialization: &PublisherMaterializationResult,
        _policy_binding: &ReleasePolicyBinding,
    ) -> anyhow::Result<()> {
        if !self.accept {
            anyhow::bail!("fixture rejects release evidence");
        }
        Ok(())
    }

    fn verify_substrate_release_evidence(
        &self,
        _release: &ryeos_bundle_publication_contract::SubstrateRelease,
        _accepted_result: &ProductBuildAcceptedResult,
        _policy_binding: &ReleasePolicyBinding,
    ) -> anyhow::Result<()> {
        if !self.accept {
            anyhow::bail!("fixture rejects substrate release evidence");
        }
        Ok(())
    }
}

fn hash(byte: char) -> String {
    std::iter::repeat_n(byte, 64).collect()
}

fn binding() -> ReleasePolicyBinding {
    ReleasePolicyBinding {
        catalog_namespace: "official".into(),
        bundle_publication_policy_section_digest: hash('8'),
        trust_epoch: 1,
    }
}

fn manifest(path: &str, blob: &str) -> Value {
    json!({
        "schema": EXTERNAL_CONTENT_TREE_SCHEMA,
        "kind": EXTERNAL_CONTENT_MANIFEST_KIND,
        "entries": [{"path": path, "kind": "file", "mode": 420, "blob_hash": blob, "size": 1}],
        "entry_count": 1,
        "total_bytes": 1
    })
}

fn fixture() -> (MemoryObjects, BundleGeneration) {
    let mut objects = MemoryObjects::default();
    let witness = hash('a');
    let accepted_hash = objects.insert(json!({
        "schema": PRODUCT_BUILD_ACCEPTED_RESULT_SCHEMA,
        "kind": PRODUCT_BUILD_ACCEPTED_RESULT_KIND,
        "owner_principal": format!("fp:{}", hash('b')),
        "producer_ref": "directive:build/example",
        "producer_project_snapshot_hash": hash('c'),
        "producer_effective_definition_digest": hash('d'),
        "producer_parameters_digest": hash('e'),
        "producer_partition_identity": hash('f'),
        "products": [{"product_name": "bundle", "witness_hash": witness, "qualification_hash": null}]
    }));
    let input_hash = objects.insert(manifest("input", &hash('1')));
    let output_hash = objects.insert(manifest("output", &hash('2')));
    let manifest_item_hash = hash('3');
    let materialization = PublisherMaterializationResult {
        schema: PUBLISHER_MATERIALIZATION_RESULT_SCHEMA.into(),
        kind: PUBLISHER_MATERIALIZATION_RESULT_KIND.into(),
        accepted_product_result_hash: accepted_hash.clone(),
        selected_product_identity: "bundle".into(),
        selected_product_witness: witness.clone(),
        input_content_manifest_hash: input_hash,
        output_content_manifest_hash: output_hash.clone(),
        output_manifest_item_hash: manifest_item_hash.clone(),
        publisher_fingerprint: hash('4'),
        publisher_tool_effective_definition_digest: hash('5'),
        publisher_tool_artifact_identity_hash: hash('6'),
        mutation_contract: PublisherMutationContract::RyeosBundleSignV1,
    };
    let materialization_hash = objects.insert(materialization.to_value().unwrap());
    let generation = BundleGeneration {
        schema: BUNDLE_GENERATION_SCHEMA.into(),
        kind: BUNDLE_GENERATION_KIND.into(),
        bundle_name: "example".into(),
        authored_version: "1.0.0".into(),
        content_manifest_hash: output_hash,
        manifest_item_hash,
        target: BundleTarget::Portable,
        build_profile: BundleBuildProfile::Release,
        substrate_protocol: 1,
        bundle_manifest_format: "ryeos.bundle-manifest/v1".into(),
        accepted_product_result_hash: accepted_hash.clone(),
        selected_product_identity: "bundle".into(),
        selected_product_witness: witness.clone(),
        publisher_materialization_result_hash: materialization_hash,
        accepted_capture_result_hash: accepted_hash.clone(),
        selected_signed_product_identity: "bundle".into(),
        selected_signed_product_witness: witness.clone(),
        source_snapshot_hash: None,
        qualification_evidence_hashes: vec![],
        provenance_hash: None,
        sbom_hash: None,
    };
    (objects, generation)
}

#[test]
fn verifies_exact_cross_object_identity_and_requires_proof() {
    let (objects, generation) = fixture();
    let proof = Proof {
        called: AtomicBool::new(false),
        accept: true,
    };
    let verified =
        verify_bundle_generation(generation, &objects, &proof, &proof, &binding()).unwrap();
    assert!(proof.called.load(Ordering::Relaxed));
    assert_eq!(
        verified.accepted_result().products[0].product_name,
        "bundle"
    );
    assert_eq!(
        verified.materialization().output_manifest().entries[0].path,
        "output"
    );
}

#[test]
fn rejects_selected_witness_mismatch_before_proof() {
    let (objects, mut generation) = fixture();
    generation.selected_product_witness = hash('9');
    let proof = Proof {
        called: AtomicBool::new(false),
        accept: true,
    };
    let error =
        verify_bundle_generation(generation, &objects, &proof, &proof, &binding()).unwrap_err();
    assert!(error.to_string().contains("witness disagrees"));
    assert!(!proof.called.load(Ordering::Relaxed));
}

#[test]
fn rejects_generation_materialization_output_mismatch() {
    let (objects, mut generation) = fixture();
    generation.manifest_item_hash = hash('9');
    let proof = Proof {
        called: AtomicBool::new(false),
        accept: true,
    };
    let error =
        verify_bundle_generation(generation, &objects, &proof, &proof, &binding()).unwrap_err();
    assert!(error.to_string().contains("output identity"));
    assert!(!proof.called.load(Ordering::Relaxed));
}

#[test]
fn rejects_unproven_publisher_transformation() {
    let (objects, generation) = fixture();
    let proof = Proof {
        called: AtomicBool::new(false),
        accept: false,
    };
    let error =
        verify_bundle_generation(generation, &objects, &proof, &proof, &binding()).unwrap_err();
    assert!(error.to_string().contains("closed mutation is unproven"));
    assert!(proof.called.load(Ordering::Relaxed));
}
