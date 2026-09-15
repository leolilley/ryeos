use super::*;
use std::collections::BTreeMap;

use crate::ignore::{IgnoreConfig, IgnoreMatcher};
use crate::objects::{ProjectFile, ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree};
use crate::project_materialization::VerifiedProjectSnapshotClosure;
use crate::signer::TestSigner;

fn declaration() -> ProductDeclaration {
    ProductDeclaration {
        name: "runtime".into(),
        source: ProductSource::RetainedProject {},
        path: "products/runtime".into(),
        shape: ProductShape::Tree,
        storage: ProductStorage::Content,
        required: true,
        bounds: ProductBounds {
            maximum_entries: 32,
            maximum_depth: 8,
            maximum_file_bytes: 1024,
            maximum_total_bytes: 4096,
        },
        expected_manifest_hash: None,
    }
}

fn declarations() -> ProductDeclarations {
    ProductDeclarations {
        schema: PRODUCT_DECLARATIONS_SCHEMA.into(),
        output_roots: Vec::new(),
        products: vec![declaration()],
    }
}

#[test]
fn authored_product_configs_decode_through_the_exact_state_contract() {
    let fixtures = [
        (
            "authoring-build-support-products",
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../../.ai/config/development/ryeos/authoring-build-support-products.yaml"
            )),
        ),
        (
            "authoring-built-utilities-products",
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../../.ai/config/development/ryeos/authoring-built-utilities-products.yaml"
            )),
        ),
        (
            "authoring-environment-products",
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../../.ai/config/development/ryeos/authoring-environment-products.yaml"
            )),
        ),
        (
            "authoring-prepared-input-products",
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../../.ai/config/development/ryeos/authoring-prepared-input-products.yaml"
            )),
        ),
        (
            "gnu-python-products",
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../../.ai/config/development/ryeos/gnu-python-products.yaml"
            )),
        ),
    ];
    for (name, source) in fixtures {
        let document: serde_yaml::Value =
            serde_yaml::from_str(source).unwrap_or_else(|error| panic!("{name}: {error}"));
        let block = serde_json::to_value(&document["build_products"])
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        ProductDeclarations::from_value(block).unwrap_or_else(|error| panic!("{name}: {error:#}"));
    }
}

fn producer() -> ProductProducerAdmission {
    ProductProducerAdmission {
        canonical_ref: "tool:test/build".into(),
        effective_definition_digest: "1".repeat(64),
        exact_program_hash: "2".repeat(64),
        producer_project_snapshot_hash: "3".repeat(64),
        launch_authority_digest: "4".repeat(64),
        admitted_parameters_digest: "5".repeat(64),
    }
}

fn ignores(patterns: &[&str]) -> IgnoreMatcher {
    IgnoreMatcher::from_config(&IgnoreConfig {
        patterns: patterns.iter().map(|p| (*p).to_owned()).collect(),
    })
    .unwrap()
}

fn node_bounds() -> crate::LargeContentCaptureBounds {
    crate::LargeContentCaptureBounds {
        max_entries: 64,
        max_depth: 16,
        max_file_bytes: 4096,
        max_total_bytes: 8192,
    }
}

fn snapshot(cas: &lillux::CasStore) -> VerifiedProjectSnapshotClosure {
    let files = [
        ("products/runtime/bin/program", b"program".as_slice(), true),
        ("products/runtime/share/empty", b"".as_slice(), false),
        (
            "products/runtime-other/scratch",
            b"not a product".as_slice(),
            false,
        ),
    ]
    .into_iter()
    .map(|(path, bytes, executable)| {
        let file = ProjectFile {
            blob_hash: cas.store_blob(bytes).unwrap(),
            size: bytes.len() as u64,
            normalized_mode: if executable {
                ProjectFile::EXECUTABLE_MODE
            } else {
                ProjectFile::REGULAR_MODE
            },
        };
        (path.to_owned(), cas.store_object(&file.to_value()).unwrap())
    })
    .collect::<BTreeMap<_, _>>();
    let tree = ProjectTree { files };
    let policy = ProjectSnapshotPolicy::from_matcher(
        crate::project_sync::ProjectSyncScope::FullProject,
        &ignores(&[]),
    )
    .unwrap();
    let snapshot = ProjectSnapshot {
        project_tree_hash: cas.store_object(&tree.to_value()).unwrap(),
        effective_policy_hash: cas.store_object(&policy.to_value()).unwrap(),
        parent_hashes: vec![],
        created_at: "2026-09-07T00:00:00Z".into(),
        source: "test".into(),
        message: None,
    };
    VerifiedProjectSnapshotClosure::load(cas, &cas.store_object(&snapshot.to_value()).unwrap())
        .unwrap()
}

fn evidence(manifest_hash: String) -> ProductCaptureEvidence {
    ProductCaptureEvidence {
        schema: PRODUCT_CAPTURE_EVIDENCE_SCHEMA,
        owner_principal: format!("fp:{}", "a".repeat(64)),
        chain_root_id: "T-root".into(),
        thread_id: "T-terminal".into(),
        admitted_launch_capsule_hash: "b".repeat(64),
        producer: producer(),
        root_producer: producer(),
        result_project_snapshot_hash: "c".repeat(64),
        workspace_output_capture_hash: None,
        producer_partition_identity: None,
        recipe_binding: "build_recipe".into(),
        recipe_ref: "config:test/build".into(),
        recipe_raw_content_digest: "d".repeat(64),
        declarations: declarations(),
        declarations_hash: declarations().content_hash().unwrap(),
        relationships: super::composition::ProductRelationships::empty(),
        declaration: declaration(),
        capture_policy_digest: "e".repeat(64),
        manifest_hash,
        manifest_kind: crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND.into(),
        entry_count: 4,
        total_bytes: 7,
    }
}

#[test]
fn producer_projection_is_closed_canonical_and_parameter_secret_free() {
    producer().validate().unwrap();
    let value = serde_json::to_value(producer()).unwrap();
    assert_eq!(
        value.get("admitted_parameters_digest"),
        Some(&serde_json::json!("5".repeat(64)))
    );
    assert!(!value.to_string().contains("sensitive parameter value"));

    let mut changed = producer();
    changed.canonical_ref = "tool:test/build@latest".into();
    assert!(changed.validate().is_err());
    changed = producer();
    changed.launch_authority_digest = "A".repeat(64);
    assert!(changed.validate().is_err());
    changed = producer();
    changed.admitted_parameters_digest = "not-a-digest".into();
    assert!(changed.validate().is_err());

    let mut extra = value;
    extra["parameters"] = serde_json::json!({"token": "sensitive parameter value"});
    assert!(serde_json::from_value::<ProductProducerAdmission>(extra).is_err());
}

#[test]
fn declarations_are_bounded_closed_and_allow_explicit_subtrees() {
    let mut contract = declarations();
    let mut subtree = declaration();
    subtree.name = "executable".into();
    subtree.path = "products/runtime/bin/program".into();
    subtree.shape = ProductShape::File;
    contract.products.push(subtree.clone());
    contract.validate().unwrap();
    assert!(contract.select("not_declared").is_err());
    contract.products.push(subtree);
    assert!(contract.validate().is_err());
    let mut value = serde_json::to_value(declarations()).unwrap();
    value["products"][0]["execute"] = serde_json::json!("arbitrary shell");
    assert!(ProductDeclarations::from_value(value).is_err());
}

#[test]
fn product_source_is_required_closed_and_workspace_products_name_one_root() {
    let mut value = serde_json::to_value(declaration()).unwrap();
    value.as_object_mut().unwrap().remove("source");
    assert!(serde_json::from_value::<ProductDeclaration>(value).is_err());

    let mut value = serde_json::to_value(declaration()).unwrap();
    value["source"] = serde_json::json!({"kind": "retained_project", "root": "extra"});
    assert!(serde_json::from_value::<ProductDeclaration>(value).is_err());

    let root = crate::objects::WorkspaceOutputRootDeclaration {
        name: "distribution".into(),
        path: "products/distribution".into(),
        storage: ProductStorage::Content,
        bounds: declaration().bounds,
    };
    let mut workspace = declaration();
    workspace.source = ProductSource::WorkspaceOutput {
        root: "distribution".into(),
    };
    workspace.path = "products/distribution/runtime".into();
    let contract = ProductDeclarations {
        schema: PRODUCT_DECLARATIONS_SCHEMA.into(),
        output_roots: vec![root],
        products: vec![workspace],
    };
    contract.validate().unwrap();

    let mut wrong = contract;
    wrong.products[0].source = ProductSource::WorkspaceOutput {
        root: "missing".into(),
    };
    assert!(wrong.validate().is_err());
}

#[test]
fn product_testimony_source_requires_the_exact_nullable_output_capture_shape() {
    let mut retained = evidence("f".repeat(64));
    retained.workspace_output_capture_hash = Some("9".repeat(64));
    assert!(retained.validate().is_err());

    let root = crate::objects::WorkspaceOutputRootDeclaration {
        name: "distribution".into(),
        path: "products/distribution".into(),
        storage: ProductStorage::Content,
        bounds: declaration().bounds,
    };
    let mut workspace = declaration();
    workspace.source = ProductSource::WorkspaceOutput {
        root: "distribution".into(),
    };
    workspace.path = "products/distribution/runtime".into();
    let declarations = ProductDeclarations {
        schema: PRODUCT_DECLARATIONS_SCHEMA.into(),
        output_roots: vec![root],
        products: vec![workspace.clone()],
    };
    let mut workspace_evidence = evidence("f".repeat(64));
    workspace_evidence.declarations_hash = declarations.content_hash().unwrap();
    workspace_evidence.declarations = declarations;
    workspace_evidence.declaration = workspace;
    assert!(workspace_evidence.validate().is_err());
    workspace_evidence.workspace_output_capture_hash = Some("9".repeat(64));
    assert!(workspace_evidence.validate().is_err());
    workspace_evidence.producer_partition_identity = Some("8".repeat(64));
    workspace_evidence.validate().unwrap();

    let mut value = serde_json::to_value(workspace_evidence).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .remove("workspace_output_capture_hash");
    assert!(serde_json::from_value::<ProductCaptureEvidence>(value).is_err());
}

#[test]
fn rejects_traversal_placeholders_and_incoherent_bounds() {
    for path in [
        "../escape",
        "/absolute",
        "products/../escape",
        "products//runtime",
    ] {
        let mut product = declaration();
        product.path = path.into();
        assert!(product.validate().is_err(), "{path}");
    }
    let mut product = declaration();
    product.expected_manifest_hash = Some("pending".into());
    assert!(product.validate().is_err());
    product.expected_manifest_hash = None;
    product.bounds.maximum_file_bytes = 8192;
    assert!(product.validate().is_err());
}

#[test]
fn node_policy_can_only_narrow_declared_bounds() {
    let mut node = node_bounds();
    node.max_total_bytes = 512;
    node.max_file_bytes = 256;
    let effective = declaration().bounds.intersect(&node).unwrap();
    assert_eq!(effective.max_entries, 32);
    assert_eq!(effective.max_depth, 8);
    assert_eq!(effective.max_total_bytes, 512);
    assert_eq!(effective.max_file_bytes, 256);
    node.max_entries = 0;
    assert!(declaration().bounds.intersect(&node).is_err());
}

#[test]
fn capture_preserves_empty_files_and_modes_without_sibling_scratch() {
    let temp = tempfile::tempdir().unwrap();
    let cas = lillux::CasStore::new(temp.path().join("cas"));
    let snapshot = snapshot(&cas);
    let selection = declaration()
        .select_retained(&snapshot, &ignores(&[]), &node_bounds())
        .unwrap()
        .unwrap();
    let manifest = selection.content_manifest(&cas).unwrap();
    assert_eq!(selection.total_bytes(), 7);
    assert_eq!(selection.entry_count(), 4);
    let program = manifest
        .entries
        .iter()
        .find(|entry| entry.path == "bin/program")
        .unwrap();
    assert_eq!(program.mode, Some(ProjectFile::EXECUTABLE_MODE));
    let empty = manifest
        .entries
        .iter()
        .find(|entry| entry.path == "share/empty")
        .unwrap();
    assert_eq!(empty.size, Some(0));
    assert!(
        manifest
            .entries
            .iter()
            .all(|entry| !entry.path.contains("scratch"))
    );
    let mut required = declaration();
    required.path = "products/missing".into();
    assert!(
        required
            .select_retained(&snapshot, &ignores(&[]), &node_bounds())
            .is_err()
    );
    required.required = false;
    assert!(
        required
            .select_retained(&snapshot, &ignores(&[]), &node_bounds())
            .unwrap()
            .is_none()
    );
}

#[test]
fn optional_does_not_hide_shape_exclusion_or_budget_failures() {
    let temp = tempfile::tempdir().unwrap();
    let cas = lillux::CasStore::new(temp.path().join("cas"));
    let snapshot = snapshot(&cas);
    let mut product = declaration();
    product.required = false;
    product.shape = ProductShape::File;
    assert!(
        product
            .select_retained(&snapshot, &ignores(&[]), &node_bounds())
            .is_err()
    );
    product.shape = ProductShape::Tree;
    assert!(
        product
            .select_retained(&snapshot, &ignores(&["products/runtime"]), &node_bounds())
            .is_err()
    );
    product.bounds.maximum_file_bytes = 1;
    assert!(
        product
            .select_retained(&snapshot, &ignores(&[]), &node_bounds())
            .is_err()
    );
}

#[test]
fn missing_optional_product_cannot_hide_an_excluded_ancestor() {
    let temp = tempfile::tempdir().unwrap();
    let cas = lillux::CasStore::new(temp.path().join("cas"));
    let snapshot = snapshot(&cas);
    let mut product = declaration();
    product.required = false;
    product.path = "products/runtime/missing".into();
    // A basename glob matches the ancestor, not the complete locator. The
    // absent path must still pass the ordinary retained-selection policy.
    let policy = ignores(&["runtime"]);
    assert!(!policy.is_ignored(&product.path));
    assert!(policy.is_ignored("products/runtime"));
    for shape in [ProductShape::File, ProductShape::Tree] {
        product.shape = shape;
        assert!(
            product
                .select_retained(&snapshot, &ignores(&[]), &node_bounds())
                .unwrap()
                .is_none()
        );
        let error = product
            .select_retained(&snapshot, &policy, &node_bounds())
            .err()
            .expect("excluded ancestor must refuse even absent optional output");
        assert!(error.to_string().contains("excluded ancestor"));
    }
}

#[test]
fn testimony_uses_existing_signature_and_roots_only_selected_content() {
    let temp = tempfile::tempdir().unwrap();
    let cas = lillux::CasStore::new(temp.path().join("cas"));
    let snapshot = snapshot(&cas);
    let product = declaration()
        .select_retained(&snapshot, &ignores(&[]), &node_bounds())
        .unwrap()
        .unwrap();
    let manifest_hash = cas
        .store_object(&serde_json::to_value(product.content_manifest(&cas).unwrap()).unwrap())
        .unwrap();
    let evidence = evidence(manifest_hash.clone());
    let signer = TestSigner::new();
    let attestation = evidence
        .sign_attestation(&signer, "2026-09-07T00:00:00Z".into())
        .unwrap();
    attestation
        .verify_with_key(&signer.verifying_key())
        .unwrap();
    ProductCaptureEvidence::verify_attestation_for_owner(
        &attestation,
        &signer.verifying_key(),
        &evidence.owner_principal,
    )
    .unwrap();
    assert!(
        ProductCaptureEvidence::verify_attestation_for_owner(
            &attestation,
            &signer.verifying_key(),
            &format!("fp:{}", "1".repeat(64))
        )
        .is_err()
    );
    assert_eq!(
        ProductCaptureEvidence::from_attestation(&attestation).unwrap(),
        evidence
    );
    let hash = cas.store_object(&attestation.to_value()).unwrap();
    let closure = crate::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [hash.clone()],
        crate::object_closure::ObjectClosureLimits::default(),
    )
    .unwrap();
    assert!(closure.is_complete(), "{closure:?}");
    assert_eq!(closure.object_hashes.len(), 2);
    assert!(closure.object_hashes.contains(&manifest_hash));
    assert_eq!(closure.blob_hashes.len(), 2);
    assert!(!closure.object_hashes.contains(snapshot.snapshot_hash()));
    // Historical coordinates deliberately do not need to exist to verify the
    // node's retained testimony. Arbitrary edits still invalidate its signature.
    let mut tampered = attestation;
    tampered.evidence["producer"]["exact_program_hash"] = serde_json::json!("6".repeat(64));
    assert!(tampered.verify_with_key(&signer.verifying_key()).is_err());
}

#[test]
fn testimony_refuses_wrong_subject_claim_storage_and_expected_manifest() {
    let signer = TestSigner::new();
    let original = evidence("f".repeat(64));
    let mut attestation = original
        .sign_attestation(&signer, "2026-09-07T00:00:00Z".into())
        .unwrap();
    attestation.subject_hash = "1".repeat(64);
    assert!(ProductCaptureEvidence::from_attestation(&attestation).is_err());
    let mut wrong = original.clone();
    wrong.manifest_kind = "project_snapshot".into();
    assert!(wrong.validate().is_err());
    wrong = original;
    wrong.declaration.expected_manifest_hash = Some("1".repeat(64));
    assert!(wrong.validate().is_err());
}

#[test]
fn testimony_cannot_restate_a_different_product_declaration() {
    let mut witness = evidence("f".repeat(64));
    witness.declaration.path = "products/unrelated".into();
    assert!(witness.validate().is_err());
    let mut witness = evidence("f".repeat(64));
    witness.declarations.products[0].path = "products/unrelated".into();
    assert!(witness.validate().is_err());
}

#[test]
fn file_products_cannot_overlap_descendant_selections() {
    let mut contract = declarations();
    contract.products[0].shape = ProductShape::File;
    let mut child = declaration();
    child.name = "child".into();
    child.path = "products/runtime/child".into();
    contract.products.push(child);
    assert!(contract.validate().is_err());
}
