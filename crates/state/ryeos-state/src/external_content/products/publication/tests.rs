use std::sync::Arc;

use super::*;
use crate::objects::{
    EXTERNAL_CONTENT_TREE_SCHEMA, EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
    EXTERNAL_LARGE_CONTENT_SCHEMA, ExternalContentManifestEntry, ExternalContentManifestEntryKind,
    ExternalContentManifestObject, ExternalLargeContentManifestEntry,
    ExternalLargeContentManifestObject,
};
use crate::signer::TestSigner;
use crate::{StateDb, TrustStore};

fn declaration() -> super::super::ProductDeclaration {
    super::super::ProductDeclaration {
        name: "runtime".into(),
        source: super::super::ProductSource::RetainedProject {},
        path: "products/runtime".into(),
        shape: ProductShape::Tree,
        storage: super::super::ProductStorage::Content,
        required: true,
        bounds: super::super::ProductBounds {
            maximum_entries: 8,
            maximum_depth: 4,
            maximum_file_bytes: 1024,
            maximum_total_bytes: 4096,
        },
        expected_manifest_hash: None,
    }
}

pub(in crate::external_content::products) fn store_manifest(
    authority: &PinnedStateAuthority,
) -> String {
    store_ordinary_manifest(authority, "program")
}

fn store_ordinary_manifest(authority: &PinnedStateAuthority, path: &str) -> String {
    let cas = authority.cas_store().unwrap();
    let bytes = b"program";
    let blob_hash = cas.store_blob(bytes).unwrap();
    let manifest = ExternalContentManifestObject {
        schema: EXTERNAL_CONTENT_TREE_SCHEMA.into(),
        kind: EXTERNAL_CONTENT_MANIFEST_KIND.into(),
        entries: vec![ExternalContentManifestEntry {
            path: path.into(),
            kind: ExternalContentManifestEntryKind::File,
            mode: Some(0o755),
            blob_hash: Some(blob_hash),
            size: Some(bytes.len() as u64),
            target: None,
        }],
        entry_count: 1,
        total_bytes: bytes.len() as u64,
    };
    cas.store_object(&serde_json::to_value(&manifest).unwrap())
        .unwrap()
}

fn store_large_manifest(authority: &PinnedStateAuthority, entries: &[(&str, bool)]) -> String {
    let cas = authority.cas_store().unwrap();
    let bytes = b"program";
    let blob_hash = cas.store_blob(bytes).unwrap();
    let entries = entries
        .iter()
        .map(|(path, directory)| {
            let directory = *directory;
            ExternalLargeContentManifestEntry {
                path: (*path).into(),
                kind: if directory {
                    ExternalContentManifestEntryKind::Dir
                } else {
                    ExternalContentManifestEntryKind::File
                },
                mode: (!directory).then_some(0o755),
                blob_hash: (!directory).then(|| blob_hash.clone()),
                file_sha256: None,
                size: (!directory).then_some(bytes.len() as u64),
                chunk_size: None,
                chunk_hashes: Vec::new(),
                target: None,
            }
        })
        .collect::<Vec<_>>();
    let manifest = ExternalLargeContentManifestObject {
        schema: EXTERNAL_LARGE_CONTENT_SCHEMA.into(),
        kind: EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.into(),
        entry_count: entries.len(),
        total_bytes: bytes.len() as u64,
        entries,
    };
    cas.store_object(&serde_json::to_value(&manifest).unwrap())
        .unwrap()
}

fn store_rich_tree_manifest(
    authority: &PinnedStateAuthority,
    storage: super::super::ProductStorage,
) -> (String, &'static str) {
    let cas = authority.cas_store().unwrap();
    let bytes = b"program";
    let blob_hash = cas.store_blob(bytes).unwrap();
    match storage {
        super::super::ProductStorage::Content => {
            let manifest = ExternalContentManifestObject {
                schema: EXTERNAL_CONTENT_TREE_SCHEMA.into(),
                kind: EXTERNAL_CONTENT_MANIFEST_KIND.into(),
                entries: vec![
                    ExternalContentManifestEntry {
                        path: "bin".into(),
                        kind: ExternalContentManifestEntryKind::Dir,
                        mode: None,
                        blob_hash: None,
                        size: None,
                        target: None,
                    },
                    ExternalContentManifestEntry {
                        path: "bin/program".into(),
                        kind: ExternalContentManifestEntryKind::File,
                        mode: Some(0o755),
                        blob_hash: Some(blob_hash),
                        size: Some(bytes.len() as u64),
                        target: None,
                    },
                    ExternalContentManifestEntry {
                        path: "bin/run".into(),
                        kind: ExternalContentManifestEntryKind::Symlink,
                        mode: None,
                        blob_hash: None,
                        size: None,
                        target: Some("program".into()),
                    },
                    ExternalContentManifestEntry {
                        path: "empty".into(),
                        kind: ExternalContentManifestEntryKind::Dir,
                        mode: None,
                        blob_hash: None,
                        size: None,
                        target: None,
                    },
                ],
                entry_count: 4,
                total_bytes: bytes.len() as u64,
            };
            (
                cas.store_object(&serde_json::to_value(manifest).unwrap())
                    .unwrap(),
                EXTERNAL_CONTENT_MANIFEST_KIND,
            )
        }
        super::super::ProductStorage::LargeContent => {
            let entries = vec![
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
                    path: "bin/program".into(),
                    kind: ExternalContentManifestEntryKind::File,
                    mode: Some(0o755),
                    blob_hash: Some(blob_hash),
                    file_sha256: None,
                    size: Some(bytes.len() as u64),
                    chunk_size: None,
                    chunk_hashes: Vec::new(),
                    target: None,
                },
                ExternalLargeContentManifestEntry {
                    path: "bin/run".into(),
                    kind: ExternalContentManifestEntryKind::Symlink,
                    mode: None,
                    blob_hash: None,
                    file_sha256: None,
                    size: None,
                    chunk_size: None,
                    chunk_hashes: Vec::new(),
                    target: Some("program".into()),
                },
                ExternalLargeContentManifestEntry {
                    path: "empty".into(),
                    kind: ExternalContentManifestEntryKind::Dir,
                    mode: None,
                    blob_hash: None,
                    file_sha256: None,
                    size: None,
                    chunk_size: None,
                    chunk_hashes: Vec::new(),
                    target: None,
                },
            ];
            let manifest = ExternalLargeContentManifestObject {
                schema: EXTERNAL_LARGE_CONTENT_SCHEMA.into(),
                kind: EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.into(),
                entry_count: entries.len(),
                total_bytes: bytes.len() as u64,
                entries,
            };
            (
                cas.store_object(&serde_json::to_value(manifest).unwrap())
                    .unwrap(),
                EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
            )
        }
    }
}

pub(in crate::external_content::products) fn evidence(
    manifest_hash: String,
    thread_id: &str,
) -> ProductCaptureEvidence {
    let declarations = super::super::ProductDeclarations {
        schema: super::super::PRODUCT_DECLARATIONS_SCHEMA.into(),
        output_roots: Vec::new(),
        products: vec![declaration()],
    };
    ProductCaptureEvidence {
        schema: super::super::PRODUCT_CAPTURE_EVIDENCE_SCHEMA,
        owner_principal: format!("fp:{}", "a".repeat(64)),
        chain_root_id: "T-build-root".into(),
        thread_id: thread_id.into(),
        admitted_launch_capsule_hash: "b".repeat(64),
        root_producer: super::super::ProductProducerAdmission {
            canonical_ref: "tool:test/build".into(),
            effective_definition_digest: "1".repeat(64),
            exact_program_hash: "2".repeat(64),
            producer_project_snapshot_hash: "3".repeat(64),
            launch_authority_digest: "4".repeat(64),
            admitted_parameters_digest: "5".repeat(64),
        },
        producer: super::super::ProductProducerAdmission {
            canonical_ref: "tool:test/build".into(),
            effective_definition_digest: "1".repeat(64),
            exact_program_hash: "2".repeat(64),
            producer_project_snapshot_hash: "3".repeat(64),
            launch_authority_digest: "4".repeat(64),
            admitted_parameters_digest: "5".repeat(64),
        },
        result_project_snapshot_hash: "c".repeat(64),
        workspace_output_capture_hash: None,
        producer_partition_identity: None,
        recipe_binding: "build_recipe".into(),
        recipe_ref: "config:test/build".into(),
        recipe_raw_content_digest: "d".repeat(64),
        declarations_hash: declarations.content_hash().unwrap(),
        declarations,
        relationships: super::super::composition::ProductRelationships::empty(),
        declaration: declaration(),
        capture_policy_digest: "e".repeat(64),
        manifest_hash,
        manifest_kind: EXTERNAL_CONTENT_MANIFEST_KIND.into(),
        entry_count: 1,
        total_bytes: 7,
    }
}

fn replace_declaration(
    evidence: &mut ProductCaptureEvidence,
    declaration: super::super::ProductDeclaration,
) {
    evidence.declaration = declaration.clone();
    evidence.declarations.products = vec![declaration];
    evidence.declarations_hash = evidence.declarations.content_hash().unwrap();
}

fn workspace_output_evidence(
    manifest_hash: String,
    manifest_kind: &str,
    storage: super::super::ProductStorage,
    thread_id: &str,
) -> ProductCaptureEvidence {
    let mut declaration = declaration();
    declaration.source = super::super::ProductSource::WorkspaceOutput {
        root: "distribution".into(),
    };
    declaration.storage = storage;
    let declarations = super::super::ProductDeclarations {
        schema: super::super::PRODUCT_DECLARATIONS_SCHEMA.into(),
        output_roots: vec![crate::objects::WorkspaceOutputRootDeclaration {
            name: "distribution".into(),
            path: "products".into(),
            storage,
            bounds: declaration.bounds.clone(),
        }],
        products: vec![declaration.clone()],
    };
    let mut result = evidence(manifest_hash, thread_id);
    result.workspace_output_capture_hash = Some("9".repeat(64));
    result.producer_partition_identity = Some("8".repeat(64));
    result.declaration = declaration;
    result.declarations_hash = declarations.content_hash().unwrap();
    result.declarations = declarations;
    result.manifest_kind = manifest_kind.into();
    result.entry_count = 4;
    result
}

pub(in crate::external_content::products) fn trust(signer: &TestSigner) -> Arc<TrustStore> {
    let mut trust = TrustStore::new();
    trust.insert(signer.fingerprint().to_owned(), signer.verifying_key());
    Arc::new(trust)
}

fn found(lookup: ProductWitnessLookup) -> VerifiedProductWitness {
    match lookup {
        ProductWitnessLookup::Found(witness) => witness,
        ProductWitnessLookup::Missing => panic!("expected product witness"),
    }
}

#[test]
fn durable_retry_returns_incumbent_and_refuses_contradictory_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let signer = TestSigner::new();
    let trust = trust(&signer);
    let db = StateDb::open(temp.path(), Arc::clone(&trust)).unwrap();
    let authority = db.pinned_authority().unwrap();
    let guard = authority.acquire_shared_guard().unwrap();
    let evidence = evidence(store_manifest(&authority), "T-build");
    let coordinate = ProductCaptureCoordinate::from_evidence(&evidence).unwrap();

    assert_eq!(
        lookup_product_witness_guarded(
            &authority,
            &coordinate,
            &signer.verifying_key(),
            ObjectClosureLimits::default(),
            &guard,
        )
        .unwrap(),
        ProductWitnessLookup::Missing
    );
    let first_attestation = evidence
        .sign_attestation(&signer, "2026-09-07T00:00:00Z".into())
        .unwrap();
    let first = publish_product_witness(
        &authority,
        &coordinate,
        &first_attestation,
        ObjectClosureLimits::default(),
        &signer,
        &guard,
    )
    .unwrap();
    assert!(!first.reused_existing);

    let retry_attestation = evidence
        .sign_attestation(&signer, "2026-09-07T00:00:01Z".into())
        .unwrap();
    let retry = publish_product_witness(
        &authority,
        &coordinate,
        &retry_attestation,
        ObjectClosureLimits::default(),
        &signer,
        &guard,
    )
    .unwrap();
    assert!(retry.reused_existing);
    assert_eq!(
        retry.witness.attestation_hash,
        first.witness.attestation_hash
    );
    assert_eq!(
        retry.witness.attestation.issued_at,
        first_attestation.issued_at
    );

    let mut contradiction = evidence.clone();
    contradiction.producer.exact_program_hash = "f".repeat(64);
    let contradiction = contradiction
        .sign_attestation(&signer, "2026-09-07T00:00:02Z".into())
        .unwrap();
    let error = publish_product_witness(
        &authority,
        &coordinate,
        &contradiction,
        ObjectClosureLimits::default(),
        &signer,
        &guard,
    )
    .unwrap_err();
    assert!(error.to_string().contains("contradicts"));
}

#[test]
fn exact_owner_node_lookup_survives_reopen_without_historical_objects() {
    let temp = tempfile::tempdir().unwrap();
    let signer = TestSigner::new();
    let trust = trust(&signer);
    let db = StateDb::open(temp.path(), Arc::clone(&trust)).unwrap();
    let authority = db.pinned_authority().unwrap();
    let guard = authority.acquire_shared_guard().unwrap();
    let evidence = evidence(store_manifest(&authority), "T-collected-history");
    let coordinate = ProductCaptureCoordinate::from_evidence(&evidence).unwrap();
    let published = publish_product_witness(
        &authority,
        &coordinate,
        &evidence
            .sign_attestation(&signer, "2026-09-07T00:00:00Z".into())
            .unwrap(),
        ObjectClosureLimits::default(),
        &signer,
        &guard,
    )
    .unwrap();
    drop(guard);
    drop(authority);
    drop(db);

    let reopened = StateDb::open(temp.path(), trust).unwrap();
    let authority = reopened.pinned_authority().unwrap();
    let witness = found(
        lookup_product_witness(
            &authority,
            &coordinate,
            &signer.verifying_key(),
            ObjectClosureLimits::default(),
        )
        .unwrap(),
    );
    assert_eq!(witness.attestation_hash, published.witness.attestation_hash);
    assert_eq!(witness.evidence, evidence);

    let mut wrong_owner = coordinate.clone();
    wrong_owner.owner_principal = format!("fp:{}", "1".repeat(64));
    assert_eq!(
        lookup_product_witness(
            &authority,
            &wrong_owner,
            &signer.verifying_key(),
            ObjectClosureLimits::default(),
        )
        .unwrap(),
        ProductWitnessLookup::Missing
    );
    assert!(
        lookup_product_witness_hash(
            &authority,
            &wrong_owner,
            &witness.attestation_hash,
            &signer.verifying_key(),
            ObjectClosureLimits::default(),
        )
        .is_err()
    );
    let other_node = lillux::crypto::SigningKey::from_bytes(&[7; 32]).verifying_key();
    assert!(
        lookup_product_witness(
            &authority,
            &coordinate,
            &other_node,
            ObjectClosureLimits::default(),
        )
        .is_err()
    );
}

#[test]
fn identical_bytes_keep_distinct_execution_witnesses_and_integrity_is_not_missing() {
    let temp = tempfile::tempdir().unwrap();
    let signer = TestSigner::new();
    let db = StateDb::open(temp.path(), trust(&signer)).unwrap();
    let authority = db.pinned_authority().unwrap();
    let guard = authority.acquire_shared_guard().unwrap();
    let manifest_hash = store_manifest(&authority);
    let first_evidence = evidence(manifest_hash.clone(), "T-attempt-one");
    let second_evidence = evidence(manifest_hash, "T-attempt-two");
    let first_coordinate = ProductCaptureCoordinate::from_evidence(&first_evidence).unwrap();
    let second_coordinate = ProductCaptureCoordinate::from_evidence(&second_evidence).unwrap();
    let publish = |evidence: &ProductCaptureEvidence, coordinate: &ProductCaptureCoordinate| {
        publish_product_witness(
            &authority,
            coordinate,
            &evidence
                .sign_attestation(&signer, "2026-09-07T00:00:00Z".into())
                .unwrap(),
            ObjectClosureLimits::default(),
            &signer,
            &guard,
        )
        .unwrap()
    };
    let first = publish(&first_evidence, &first_coordinate);
    let second = publish(&second_evidence, &second_coordinate);
    assert_ne!(
        first_coordinate.coordinate_id().unwrap(),
        second_coordinate.coordinate_id().unwrap()
    );
    assert_ne!(
        first.witness.attestation_hash,
        second.witness.attestation_hash
    );
    assert_eq!(
        first.witness.evidence.manifest_hash,
        second.witness.evidence.manifest_hash
    );

    assert_eq!(
        lookup_product_witness_hash_guarded(
            &authority,
            &first_coordinate,
            &"9".repeat(64),
            &signer.verifying_key(),
            ObjectClosureLimits::default(),
            &guard,
        )
        .unwrap(),
        ProductWitnessLookup::Missing
    );
    let mut too_small = ObjectClosureLimits::default();
    too_small.max_blob_bytes = 1;
    assert!(
        lookup_product_witness_guarded(
            &authority,
            &first_coordinate,
            &signer.verifying_key(),
            too_small,
            &guard,
        )
        .is_err()
    );
}

#[test]
fn publication_refuses_manifest_sizes_that_contradict_retained_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let signer = TestSigner::new();
    let db = StateDb::open(temp.path(), trust(&signer)).unwrap();
    let authority = db.pinned_authority().unwrap();
    let guard = authority.acquire_shared_guard().unwrap();
    let cas = authority.cas_store().unwrap();
    let blob_hash = cas.store_blob(b"program").unwrap();
    let manifest = ExternalContentManifestObject {
        schema: EXTERNAL_CONTENT_TREE_SCHEMA.into(),
        kind: EXTERNAL_CONTENT_MANIFEST_KIND.into(),
        entries: vec![ExternalContentManifestEntry {
            path: "program".into(),
            kind: ExternalContentManifestEntryKind::File,
            mode: Some(0o755),
            blob_hash: Some(blob_hash),
            size: Some(8),
            target: None,
        }],
        entry_count: 1,
        total_bytes: 8,
    };
    let manifest_hash = cas
        .store_object(&serde_json::to_value(manifest).unwrap())
        .unwrap();
    let mut evidence = evidence(manifest_hash, "T-size-mismatch");
    evidence.total_bytes = 8;
    let coordinate = ProductCaptureCoordinate::from_evidence(&evidence).unwrap();
    let error = publish_product_witness(
        &authority,
        &coordinate,
        &evidence
            .sign_attestation(&signer, "2026-09-07T00:00:00Z".into())
            .unwrap(),
        ObjectClosureLimits::default(),
        &signer,
        &guard,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("expected 8"));
}

#[test]
fn tree_product_accepts_single_content_entry_in_both_storage_tiers() {
    let temp = tempfile::tempdir().unwrap();
    let signer = TestSigner::new();
    let db = StateDb::open(temp.path(), trust(&signer)).unwrap();
    let authority = db.pinned_authority().unwrap();
    let guard = authority.acquire_shared_guard().unwrap();

    let ordinary = evidence(
        store_ordinary_manifest(&authority, crate::objects::FILE_REALIZATION_ENTRY_PATH),
        "T-tree-content",
    );
    let ordinary_coordinate = ProductCaptureCoordinate::from_evidence(&ordinary).unwrap();
    publish_product_witness(
        &authority,
        &ordinary_coordinate,
        &ordinary
            .sign_attestation(&signer, "2026-09-07T00:00:00Z".into())
            .unwrap(),
        ObjectClosureLimits::default(),
        &signer,
        &guard,
    )
    .unwrap();

    let mut large = evidence(
        store_large_manifest(
            &authority,
            &[(crate::objects::FILE_REALIZATION_ENTRY_PATH, false)],
        ),
        "T-large-tree-content",
    );
    let mut large_declaration = declaration();
    large_declaration.storage = super::super::ProductStorage::LargeContent;
    replace_declaration(&mut large, large_declaration);
    large.manifest_kind = EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.into();
    let large_coordinate = ProductCaptureCoordinate::from_evidence(&large).unwrap();
    publish_product_witness(
        &authority,
        &large_coordinate,
        &large
            .sign_attestation(&signer, "2026-09-07T00:00:00Z".into())
            .unwrap(),
        ObjectClosureLimits::default(),
        &signer,
        &guard,
    )
    .unwrap();
}

#[test]
fn workspace_output_product_publication_preserves_rich_tree_and_scrubs_real_bytes() {
    for storage in [
        super::super::ProductStorage::Content,
        super::super::ProductStorage::LargeContent,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let signer = TestSigner::new();
        let db = StateDb::open(temp.path(), trust(&signer)).unwrap();
        let authority = db.pinned_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let (manifest_hash, manifest_kind) = store_rich_tree_manifest(&authority, storage);
        let evidence = workspace_output_evidence(
            manifest_hash,
            manifest_kind,
            storage,
            match storage {
                super::super::ProductStorage::Content => "T-workspace-content",
                super::super::ProductStorage::LargeContent => "T-workspace-large",
            },
        );
        let coordinate = ProductCaptureCoordinate::from_evidence(&evidence).unwrap();
        let published = publish_product_witness(
            &authority,
            &coordinate,
            &evidence
                .sign_attestation(&signer, "2026-09-08T00:00:00Z".into())
                .unwrap(),
            ObjectClosureLimits::default(),
            &signer,
            &guard,
        )
        .unwrap();
        let observed = found(
            lookup_product_witness_guarded(
                &authority,
                &coordinate,
                &signer.verifying_key(),
                ObjectClosureLimits::default(),
                &guard,
            )
            .unwrap(),
        );
        assert_eq!(
            observed.attestation_hash,
            published.witness.attestation_hash
        );
        assert_eq!(
            observed.evidence.workspace_output_capture_hash,
            Some("9".repeat(64))
        );
        let manifest = authority
            .cas_store()
            .unwrap()
            .get_object(&observed.evidence.manifest_hash)
            .unwrap()
            .unwrap();
        assert_eq!(manifest["entries"][1]["mode"], 0o755);
        assert_eq!(manifest["entries"][2]["target"], "program");
        assert_eq!(manifest["entries"][3]["path"], "empty");
    }
}

#[test]
fn publication_applies_declaration_file_and_depth_bounds_to_manifest_entries() {
    let temp = tempfile::tempdir().unwrap();
    let signer = TestSigner::new();
    let db = StateDb::open(temp.path(), trust(&signer)).unwrap();
    let authority = db.pinned_authority().unwrap();
    let guard = authority.acquire_shared_guard().unwrap();

    let mut file_too_large = evidence(store_manifest(&authority), "T-file-bound");
    let mut narrow_file = declaration();
    narrow_file.bounds.maximum_file_bytes = 1;
    replace_declaration(&mut file_too_large, narrow_file);
    let coordinate = ProductCaptureCoordinate::from_evidence(&file_too_large).unwrap();
    let error = publish_product_witness(
        &authority,
        &coordinate,
        &file_too_large
            .sign_attestation(&signer, "2026-09-07T00:00:00Z".into())
            .unwrap(),
        ObjectClosureLimits::default(),
        &signer,
        &guard,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("declared bound at program"));

    let mut too_deep = evidence(
        store_large_manifest(&authority, &[("dir", true), ("dir/child", false)]),
        "T-depth-bound",
    );
    let mut shallow_tree = declaration();
    shallow_tree.storage = super::super::ProductStorage::LargeContent;
    shallow_tree.bounds.maximum_depth = 1;
    replace_declaration(&mut too_deep, shallow_tree);
    too_deep.manifest_kind = EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.into();
    too_deep.entry_count = 2;
    let coordinate = ProductCaptureCoordinate::from_evidence(&too_deep).unwrap();
    let error = publish_product_witness(
        &authority,
        &coordinate,
        &too_deep
            .sign_attestation(&signer, "2026-09-07T00:00:00Z".into())
            .unwrap(),
        ObjectClosureLimits::default(),
        &signer,
        &guard,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("declared depth bound at dir"));
}

#[test]
fn relationship_bounds_recheck_actual_manifest_file_and_depth_metrics_without_chunk_scrub() {
    let temp = tempfile::tempdir().unwrap();
    let signer = TestSigner::new();
    let db = StateDb::open(temp.path(), trust(&signer)).unwrap();
    let authority = db.pinned_authority().unwrap();
    let guard = authority.acquire_shared_guard().unwrap();

    let ordinary = evidence(store_manifest(&authority), "T-relationship-file-bound");
    let mut narrow_file = ordinary.declaration.bounds.clone();
    narrow_file.maximum_file_bytes = 6;
    let error = verify_product_manifest_against_bounds(
        &authority,
        &ordinary,
        &narrow_file,
        ObjectClosureLimits::default(),
        &guard,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("relationship bound at program"));

    let mut deep = evidence(
        store_large_manifest(&authority, &[("dir", true), ("dir/child", false)]),
        "T-relationship-depth-bound",
    );
    let mut declaration = declaration();
    declaration.storage = super::super::ProductStorage::LargeContent;
    replace_declaration(&mut deep, declaration);
    deep.manifest_kind = EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.into();
    deep.entry_count = 2;
    let mut narrow_depth = deep.declaration.bounds.clone();
    narrow_depth.maximum_depth = 1;
    let error = verify_product_manifest_against_bounds(
        &authority,
        &deep,
        &narrow_depth,
        ObjectClosureLimits::default(),
        &guard,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("relationship depth bound at dir"));
}

#[test]
fn witness_body_is_bounded_by_product_and_current_node_limits_before_decode() {
    let temp = tempfile::tempdir().unwrap();
    let signer = TestSigner::new();
    let db = StateDb::open(temp.path(), trust(&signer)).unwrap();
    let authority = db.pinned_authority().unwrap();
    let guard = authority.acquire_shared_guard().unwrap();
    let evidence = evidence(store_manifest(&authority), "T-wire-bound");
    let coordinate = ProductCaptureCoordinate::from_evidence(&evidence).unwrap();
    let attestation = evidence
        .sign_attestation(&signer, "2026-09-07T00:00:00Z".into())
        .unwrap();

    let mut node_limited = ObjectClosureLimits::default();
    node_limited.max_object_bytes = 1;
    let error = publish_product_witness(
        &authority,
        &coordinate,
        &attestation,
        node_limited,
        &signer,
        &guard,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("bounded wire contract"));

    let published = publish_product_witness(
        &authority,
        &coordinate,
        &attestation,
        ObjectClosureLimits::default(),
        &signer,
        &guard,
    )
    .unwrap();
    let error = lookup_product_witness_hash_guarded(
        &authority,
        &coordinate,
        &published.witness.attestation_hash,
        &signer.verifying_key(),
        node_limited,
        &guard,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("bounded wire contract"));

    let oversized_hash = authority
        .cas_store()
        .unwrap()
        .store_object(&serde_json::json!({
            "padding": "x".repeat(MAX_PRODUCT_ATTESTATION_BYTES as usize),
        }))
        .unwrap();
    let error = lookup_product_witness_hash_guarded(
        &authority,
        &coordinate,
        &oversized_hash,
        &signer.verifying_key(),
        ObjectClosureLimits::default(),
        &guard,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("bounded wire contract"));
}
