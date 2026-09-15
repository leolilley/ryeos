//! Application acceptance of previously published product testimony.
//!
//! Fixtures contain actual node-signed product witnesses and their retained
//! bytes, deliberately without historical thread/capsule objects. They exercise
//! the durable lookup/import contract, not admission or first-time capture of a
//! producer. A full producer launch must qualify that separate boundary.
mod test_state;

use std::sync::Arc;

use base64::Engine as _;
use ryeos_api::handlers::{external_content_import, external_content_products};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::identity::{AuthorizedKeyPrincipalClass, NodeIdentity, WildcardPolicy};
use ryeos_app::node_policy::sections::external_content::{
    ExternalContentImportLimits, ExternalContentImportPolicyRecord, ManagedExternalContentPolicy,
};
use ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy;
use ryeos_app::operator_external_content::products::ProductRequest;
use ryeos_app::operator_external_content::{ImportRequest, RetainedProductImportRequest};
use ryeos_app::state::AppState;
use ryeos_app::state_store::NodeIdentitySigner;
use ryeos_state::external_content::products::publication::{
    ProductCaptureCoordinate, publish_product_witness,
};
use ryeos_state::external_content::products::{
    PRODUCT_DECLARATIONS_SCHEMA, ProductBounds, ProductCaptureEvidence, ProductDeclaration,
    ProductDeclarations, ProductProducerAdmission, ProductShape, ProductStorage,
};
use serde_json::{Value, json};

const ROOT: &str = "T-00000000-0000-0000-0000-000000000001";
const TERMINAL: &str = "T-00000000-0000-0000-0000-000000000002";
const PRODUCT_BYTES: &[u8] = b"exact input";
const REMOTE_ORIGIN: &str = "site:product-source";

struct Fixture {
    _directory: tempfile::TempDir,
    state: Arc<AppState>,
    context: HandlerContext,
    evidence: ProductCaptureEvidence,
    witness_hash: String,
}

fn fixture(large_tier: bool, import_ceiling: u64, publish: bool) -> Fixture {
    fixture_with_remote_owner(large_tier, import_ceiling, publish, false)
}

fn fixture_with_remote_owner(
    large_tier: bool,
    import_ceiling: u64,
    publish: bool,
    remote_owner: bool,
) -> Fixture {
    let (directory, mut state) = test_state::build_test_state();
    let closure = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()
        .unwrap()
        .clone();
    state.node_policy = Arc::new(
        ryeos_app::node_policy::NodePolicySnapshot::from_test_records(vec![
            Arc::new(closure),
            Arc::new(ExternalContentImportPolicyRecord {
                schema: 1,
                roots: Default::default(),
                limits: ExternalContentImportLimits {
                    max_depth: 4,
                    max_entries: 16,
                    max_file_bytes: import_ceiling.min(1024),
                    max_total_bytes: import_ceiling,
                    store_budget_bytes: 8192,
                    minimum_free_bytes: 1,
                },
                managed_activation: ManagedExternalContentPolicy {
                    enabled: false,
                    limits: None,
                },
            }),
        ]),
    );
    let local_operator = NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    let operator = if remote_owner {
        NodeIdentity::create(&directory.path().join("source-operator.pem")).unwrap()
    } else {
        local_operator.clone()
    };
    if remote_owner {
        assert_ne!(operator.fingerprint(), local_operator.fingerprint());
    }
    let scopes = vec![
        "ryeos.execute.service.external-content/capture-product".to_owned(),
        "ryeos.execute.service.external-content/import".to_owned(),
        "ryeos.execute.service.external-content/product".to_owned(),
    ];
    ryeos_app::identity::reconcile_authorized_key_toml_scopes(
        &state.config.authorized_keys_dir,
        operator.fingerprint(),
        &base64::engine::general_purpose::STANDARD.encode(operator.verifying_key().as_bytes()),
        &scopes,
        "product test operator",
        state.identity.fingerprint(),
        "2026-09-07T00:00:00Z",
        &state.identity,
        WildcardPolicy::Reject,
        false,
        remote_owner.then_some(REMOTE_ORIGIN),
        false,
    )
    .unwrap();
    let context = HandlerContext::new_with_authority(
        operator.principal_id(),
        scopes,
        true,
        Some(if remote_owner {
            AuthorizedKeyPrincipalClass::RemoteOperator
        } else {
            AuthorizedKeyPrincipalClass::LocalClient
        }),
        remote_owner.then(|| REMOTE_ORIGIN.to_owned()),
    );
    let authority = state.state_store.pinned_state_authority().unwrap();
    let guard = authority.acquire_exclusive_guard(true).unwrap();
    let cas = authority.cas_store().unwrap();
    let blob = cas.store_blob(PRODUCT_BYTES).unwrap();
    let (kind, schema, storage) = if large_tier {
        (
            ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
            ryeos_state::objects::EXTERNAL_LARGE_CONTENT_SCHEMA,
            ProductStorage::LargeContent,
        )
    } else {
        (
            ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
            ryeos_state::objects::EXTERNAL_CONTENT_TREE_SCHEMA,
            ProductStorage::Content,
        )
    };
    let manifest_hash = cas
        .store_object(&json!({
            "kind": kind,
            "schema": schema,
            "entries": [{
                "path": "content", "kind": "file", "mode": 493,
                "blob_hash": blob, "size": PRODUCT_BYTES.len(),
            }],
            "entry_count": 1,
            "total_bytes": PRODUCT_BYTES.len(),
        }))
        .unwrap();
    let declaration = ProductDeclaration {
        name: "runtime".to_owned(),
        source: ryeos_state::external_content::products::ProductSource::RetainedProject {},
        path: "products/runtime".to_owned(),
        shape: ProductShape::Tree,
        storage,
        required: true,
        bounds: ProductBounds {
            maximum_entries: 16,
            maximum_depth: 4,
            maximum_file_bytes: 1024,
            maximum_total_bytes: 4096,
        },
        expected_manifest_hash: None,
    };
    let declarations = ProductDeclarations {
        schema: PRODUCT_DECLARATIONS_SCHEMA.to_owned(),
        output_roots: Vec::new(),
        products: vec![declaration.clone()],
    };
    let evidence = ProductCaptureEvidence {
        schema: ryeos_state::external_content::products::PRODUCT_CAPTURE_EVIDENCE_SCHEMA,
        owner_principal: context.fingerprint.clone(),
        chain_root_id: ROOT.to_owned(),
        thread_id: TERMINAL.to_owned(),
        // Historical coordinates are node testimony. No fake capsule is
        // installed or used to pass first-time producer capture validation.
        admitted_launch_capsule_hash: "a".repeat(64),
        root_producer: ProductProducerAdmission {
            canonical_ref: "graph:test/build".to_owned(),
            effective_definition_digest: "1".repeat(64),
            exact_program_hash: "2".repeat(64),
            producer_project_snapshot_hash: "3".repeat(64),
            launch_authority_digest: "4".repeat(64),
            admitted_parameters_digest: "5".repeat(64),
        },
        producer: ProductProducerAdmission {
            canonical_ref: "graph:test/build".to_owned(),
            effective_definition_digest: "1".repeat(64),
            exact_program_hash: "2".repeat(64),
            producer_project_snapshot_hash: "3".repeat(64),
            launch_authority_digest: "4".repeat(64),
            admitted_parameters_digest: "5".repeat(64),
        },
        result_project_snapshot_hash: "b".repeat(64),
        workspace_output_capture_hash: None,
        producer_partition_identity: None,
        recipe_binding: "product_recipe".to_owned(),
        recipe_ref: "config:test/products".to_owned(),
        recipe_raw_content_digest: "c".repeat(64),
        declarations_hash: declarations.content_hash().unwrap(),
        declarations,
        relationships:
            ryeos_state::external_content::products::composition::ProductRelationships::empty(),
        declaration,
        capture_policy_digest: "d".repeat(64),
        manifest_hash,
        manifest_kind: kind.to_owned(),
        entry_count: 1,
        total_bytes: PRODUCT_BYTES.len() as u64,
    };
    let signer = NodeIdentitySigner::from_identity(&state.identity);
    let attestation = evidence
        .sign_attestation(&signer, "2026-09-07T00:00:00Z".to_owned())
        .unwrap();
    let witness_hash = if publish {
        publish_product_witness(
            &authority,
            &ProductCaptureCoordinate::from_evidence(&evidence).unwrap(),
            &attestation,
            state
                .node_policy
                .require::<NodeObjectClosurePolicy>()
                .unwrap()
                .closure_limits()
                .unwrap(),
            &signer,
            &guard,
        )
        .unwrap()
        .witness
        .attestation_hash
    } else {
        cas.store_object(&attestation.to_value()).unwrap()
    };
    drop(guard);
    Fixture {
        _directory: directory,
        state: Arc::new(state),
        context,
        evidence,
        witness_hash,
    }
}

fn request() -> ProductRequest {
    ProductRequest {
        chain_root_id: ROOT.to_owned(),
        thread_id: TERMINAL.to_owned(),
        recipe_binding: "product_recipe".to_owned(),
        product_name: "runtime".to_owned(),
    }
}

async fn import(
    fixture: &Fixture,
    witness_hash: &str,
    maximum_bytes: u64,
) -> anyhow::Result<Value> {
    external_content_import::handle(
        ImportRequest::RetainedProduct(RetainedProductImportRequest {
            witness_hash: witness_hash.to_owned(),
            witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
            maximum_bytes,
        }),
        fixture.context.clone(),
        Arc::clone(&fixture.state),
    )
    .await
}

#[tokio::test]
async fn published_products_remain_exact_without_historical_threads_and_import_fresh_stages() {
    for large_tier in [false, true] {
        let fixture = fixture(large_tier, 4096, true);
        assert!(
            fixture
                .state
                .state_store
                .get_authoritative_thread_snapshot_with_last_event(ROOT, TERMINAL)
                .unwrap()
                .is_none()
        );
        let observed = external_content_products::get(
            request(),
            fixture.context.clone(),
            Arc::clone(&fixture.state),
        )
        .await
        .unwrap();
        assert_eq!(observed["state"], "captured");
        assert_eq!(observed["witness_hash"], fixture.witness_hash);
        assert_eq!(
            observed["evidence"]["producer"],
            serde_json::to_value(&fixture.evidence.producer).unwrap(),
        );
        let retried = external_content_products::capture(
            request(),
            fixture.context.clone(),
            Arc::clone(&fixture.state),
        )
        .await
        .unwrap();
        assert_eq!(retried, observed);
        assert!(import(&fixture, &fixture.witness_hash, 10).await.is_err());
        let first = import(&fixture, &fixture.witness_hash, 11).await.unwrap();
        let second = import(&fixture, &fixture.witness_hash, 11).await.unwrap();
        assert_ne!(first["staging_id"], second["staging_id"]);
        assert_eq!(first["request_digest"], second["request_digest"]);
        assert_eq!(first["manifest_hash"], fixture.evidence.manifest_hash);
        assert_eq!(first["total_bytes"], 11);
        let authority = fixture.state.state_store.pinned_state_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let operator = NodeIdentity::load(&fixture.state.config.operator_signing_key_path).unwrap();
        for response in [&first, &second] {
            let stage = authority
                .require_recovery()
                .unwrap()
                .open_durable_cas_upload_admitted(
                    &guard,
                    response["staging_id"].as_str().unwrap(),
                    operator.fingerprint(),
                )
                .unwrap();
            assert!(stage.admitted_target_hash().is_none());
            stage
                .ensure_protects_object(&fixture.evidence.manifest_hash)
                .unwrap();
        }
        assert!(
            fixture
                .state
                .state_store
                .with_state_db(|db| {
                    db.list_generic_head_refs(
                        ryeos_state::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE,
                    )
                })
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn testimony_is_not_a_new_import_grant_under_stricter_current_policy() {
    let fixture = fixture(false, 5, true);
    let observed = external_content_products::get(
        request(),
        fixture.context.clone(),
        Arc::clone(&fixture.state),
    )
    .await
    .unwrap();
    assert_eq!(observed["witness_hash"], fixture.witness_hash);
    // Historical testimony remains readable; admitting bytes obeys today's
    // narrower policy and the caller's own bound.
    assert!(import(&fixture, &fixture.witness_hash, 11).await.is_err());
    assert!(import(&fixture, &fixture.witness_hash, 5).await.is_err());
}

#[tokio::test]
async fn product_services_refuse_unadmitted_operator_contexts() {
    let fixture = fixture(false, 4096, true);
    let mut foreign = fixture.context.clone();
    foreign.fingerprint = format!("fp:{}", "f".repeat(64));
    let mut remote = fixture.context.clone();
    remote.authorized_key_class = Some(AuthorizedKeyPrincipalClass::RemoteOperator);
    remote.authenticated_origin_site_id = None;
    let mut unverified = fixture.context.clone();
    unverified.verified = false;
    let mut remote_node = fixture.context.clone();
    remote_node.authorized_key_class = Some(AuthorizedKeyPrincipalClass::RemoteNode);
    remote_node.authenticated_origin_site_id = Some(REMOTE_ORIGIN.to_owned());
    let mut malformed = remote.clone();
    malformed.authenticated_origin_site_id = Some(REMOTE_ORIGIN.to_owned());
    malformed.fingerprint = "fp:not-a-hash".to_owned();
    for context in [foreign, remote, unverified, remote_node, malformed] {
        assert!(
            external_content_products::get(request(), context.clone(), Arc::clone(&fixture.state))
                .await
                .is_err()
        );
        assert!(
            external_content_products::capture(
                request(),
                context.clone(),
                Arc::clone(&fixture.state),
            )
            .await
            .is_err()
        );
        let qualification = external_content_products::qualify(
            ryeos_app::operator_external_content::product_qualification::ProductQualificationRequest {
                witness_hash: fixture.witness_hash.clone(),
                witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                relationship_name: "runtime_to_consumer".to_owned(),
                verifier_chain_root_id: "T-00000000-0000-0000-0000-000000000001".to_owned(),
                verifier_thread_id: "T-00000000-0000-0000-0000-000000000002".to_owned(),
            },
            context.clone(),
            Arc::clone(&fixture.state),
        ).await.unwrap_err();
        let qualification_error = format!("{qualification:#}");
        assert!(
            qualification_error.contains("operator") || qualification_error.contains("verified"),
            "{qualification_error}"
        );
        let composition = external_content_products::compose(
            ryeos_app::operator_external_content::product_composition::ComposeRetainedProductsRequest {
                consumer_ref: "config:test/environment".to_owned(),
                project_context: Some(ryeos_app::operator_external_content::product_composition::ProductCompositionProjectContext { snapshot_hash: "a".repeat(64) }),
                selections: vec![ryeos_state::external_content::products::composition::ProductSelection {
                    declaration_id: "runtime".to_owned(),
                    witness_hash: fixture.witness_hash.clone(),
                    witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                    qualification_hash: None,
                }],
                maximum_bytes: 4096,
            },
            context.clone(),
            Arc::clone(&fixture.state),
        )
        .await
        .unwrap_err();
        // Authorization must fail before attempting to resolve this deliberately
        // nonexistent consumer generation or publishing an import stage.
        let composition_error = format!("{composition:#}");
        assert!(
            composition_error.contains("operator") || composition_error.contains("verified"),
            "{composition_error}"
        );
        assert!(
            external_content_import::handle(
                ImportRequest::RetainedProduct(RetainedProductImportRequest {
                    witness_hash: fixture.witness_hash.clone(),
                    witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                    maximum_bytes: 11,
                }),
                context,
                Arc::clone(&fixture.state),
            )
            .await
            .is_err()
        );
    }
}

#[tokio::test]
async fn admitted_remote_owner_reads_and_stages_own_product_without_ambient_authority() {
    use ryeos_app::operator_external_content::{
        BindConsumerKind, BindRequest, FilesystemImportRequest, ImportShape, ImportStorage,
    };
    let fixture = fixture_with_remote_owner(false, 4096, true, true);
    let remote = fixture.context.clone();
    // This direct-handler fixture starts after authenticated ingress. The
    // node-signed public grant is real; no target operator key is replaced.
    let admitted = ryeos_app::operator_authority::retained_admitted_operator_authority(
        &fixture.state,
        &remote.fingerprint,
        REMOTE_ORIGIN,
    )
    .unwrap();
    assert_eq!(
        admitted.principal_class,
        AuthorizedKeyPrincipalClass::RemoteOperator
    );
    assert!(
        ryeos_app::operator_authority::retained_admitted_operator_authority(
            &fixture.state,
            &remote.fingerprint,
            "site:wrong-origin"
        )
        .is_err()
    );
    let observed =
        external_content_products::get(request(), remote.clone(), Arc::clone(&fixture.state))
            .await
            .unwrap();
    assert_eq!(observed["witness_hash"], fixture.witness_hash);
    // An existing immutable witness is an exact capture retry, not evidence
    // that this fixture launched and captured a new producer.
    let retry =
        external_content_products::capture(request(), remote.clone(), Arc::clone(&fixture.state))
            .await
            .unwrap();
    assert_eq!(retry["witness_hash"], fixture.witness_hash);
    let imported = import(&fixture, &fixture.witness_hash, 11).await.unwrap();
    assert_eq!(imported["manifest_hash"], fixture.evidence.manifest_hash);
    {
        let authority = fixture.state.state_store.pinned_state_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let stage = authority
            .require_recovery()
            .unwrap()
            .open_durable_cas_upload_admitted(
                &guard,
                imported["staging_id"].as_str().unwrap(),
                remote.fingerprint.strip_prefix("fp:").unwrap(),
            )
            .unwrap();
        stage
            .ensure_protects_object(&fixture.evidence.manifest_hash)
            .unwrap();
        assert!(stage.admitted_target_hash().is_none());
    }
    let local = NodeIdentity::load(&fixture.state.config.operator_signing_key_path).unwrap();
    let foreign = HandlerContext::new_with_authority(
        local.principal_id(),
        remote.scopes.clone(),
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    );
    assert!(external_content_import::handle(
        ImportRequest::RetainedProduct(RetainedProductImportRequest {
            witness_hash: fixture.witness_hash.clone(),
            witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
            maximum_bytes: 11,
        }), foreign, Arc::clone(&fixture.state)
    ).await.is_err());
    let ambient = external_content_import::handle(
        ImportRequest::Filesystem(FilesystemImportRequest {
            root: "not-admitted".into(),
            path: "runtime".into(),
            shape: ImportShape::Tree,
            storage: ImportStorage::Content,
            maximum_bytes: 11,
            expected_file_sha256: None,
        }),
        remote.clone(),
        Arc::clone(&fixture.state),
    )
    .await
    .unwrap_err();
    assert!(format!("{ambient:#}").contains("local_client"));
    let general_bind = ryeos_app::operator_external_content::bind(
        Arc::clone(&fixture.state),
        remote,
        BindRequest {
            staging_id: imported["staging_id"].as_str().unwrap().into(),
            request_digest: imported["request_digest"].as_str().unwrap().into(),
            manifest_hash: fixture.evidence.manifest_hash.clone(),
            consumer_ref: "config:test/not-admitted".into(),
            consumer_kind: BindConsumerKind::InstalledBundle,
            project_snapshot_hash: None,
            project_path: None,
            product_selections: None,
            product_owner_principal: None,
        },
    )
    .await
    .unwrap_err();
    assert!(format!("{general_bind:#}").contains("local_client"));
}

#[test]
fn remote_owned_product_binding_requires_unchanged_current_grant() {
    use ryeos_state::objects::{ExternalContentBinding, ExternalContentConsumerAuthority};
    let fixture = fixture_with_remote_owner(false, 4096, true, true);
    let operator =
        NodeIdentity::load(&fixture._directory.path().join("source-operator.pem")).unwrap();
    let grant = ryeos_app::identity::load_verified_authorized_key(
        operator.fingerprint(),
        &fixture.state.config.authorized_keys_dir,
        &fixture.state.identity,
    )
    .unwrap()
    .unwrap();
    let product_authority =
        ryeos_app::operator_authority::admitted_operator_authority_for_principal(
            &fixture.state,
            &operator.principal_id(),
        )
        .unwrap();
    assert_eq!(product_authority.owner_principal, operator.principal_id());
    assert_eq!(product_authority.origin_site_id, REMOTE_ORIGIN);
    assert_eq!(
        product_authority.principal_class,
        AuthorizedKeyPrincipalClass::RemoteOperator
    );
    assert_eq!(product_authority.grant_digest, grant.source_file_hash);
    let consumer = ExternalContentConsumerAuthority::installed_bundle(
        "config:test/product-consumer".into(),
        "a".repeat(64),
    )
    .unwrap();
    let binding = ExternalContentBinding::active(
        fixture.evidence.manifest_hash.clone(),
        fixture.evidence.manifest_kind.clone(),
        consumer.clone(),
        fixture.state.identity.fingerprint().into(),
        operator.fingerprint().into(),
        grant.source_file_hash,
    )
    .unwrap();
    let authority = fixture.state.state_store.pinned_state_authority().unwrap();
    let guard = authority.acquire_exclusive_guard(true).unwrap();
    let cas = authority.cas_store().unwrap();
    let binding_hash = cas.store_object(&binding.to_value().unwrap()).unwrap();
    let signer = NodeIdentitySigner::from_identity(&fixture.state.identity);
    fixture
        .state
        .state_store
        .with_state_db(|db| {
            db.ensure_current_external_content_binding_epoch(&guard)?;
            db.advance_generic_head_ref(
                ryeos_state::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE,
                &binding.binding_subject_id,
                &binding_hash,
                None,
                &signer,
                &guard,
            )
        })
        .unwrap();
    drop(guard);
    let read = || {
        ryeos_app::operator_external_content::require_active_binding(
            &fixture.state,
            &cas,
            &fixture.evidence.manifest_hash,
            &consumer,
        )
    };
    read().unwrap();
    let (grant_path, _, _) = ryeos_app::identity::reconcile_authorized_key_toml_scopes(
        &fixture.state.config.authorized_keys_dir,
        operator.fingerprint(),
        &base64::engine::general_purpose::STANDARD.encode(operator.verifying_key().as_bytes()),
        &["ryeos.execute.service.external-content/product".into()],
        "narrowed remote operator",
        fixture.state.identity.fingerprint(),
        "2026-09-10T00:00:00Z",
        &fixture.state.identity,
        WildcardPolicy::Reject,
        false,
        Some(REMOTE_ORIGIN),
        false,
    )
    .unwrap();
    assert!(
        read().is_err(),
        "changed grant cannot authorize its previous binding"
    );
    std::fs::remove_file(grant_path).unwrap();
    assert!(
        ryeos_app::operator_authority::admitted_operator_authority_for_principal(
            &fixture.state,
            &operator.principal_id(),
        )
        .is_err(),
        "revoked remote product owner cannot prepare a selected consumer"
    );
    assert!(
        read().is_err(),
        "revoked grant cannot authorize retained binding bytes"
    );
}

#[test]
fn selected_launch_is_target_local_without_erasing_remote_operator_origin() {
    use ryeos_app::operator_external_content::product_composition::admit_root_product_selections;
    use ryeos_engine::contracts::{ItemSourceRoot, ItemSpace, SubjectResolutionAuthority};
    use ryeos_engine::resolution::{
        KindComposedView, ResolutionOutput, ResolutionStepName, ResolvedAncestor, TrustClass,
    };
    use ryeos_state::external_content::products::composition::{
        ProductSelection, ProductSelectionInput, ProductSelectionTarget,
    };

    let fixture = fixture_with_remote_owner(false, 4096, true, true);
    let context = ryeos_app::operator_authority::retained_admitted_operator_authority(
        &fixture.state,
        &fixture.context.fingerprint,
        REMOTE_ORIGIN,
    )
    .unwrap()
    .handler_context();
    assert_ne!(REMOTE_ORIGIN, fixture.state.threads.site_id());
    // No consumer is mocked as admitted: this deliberately untrusted unresolved
    // item proves the serving-site fence runs before source/witness admission.
    // Full selected-program success remains the installed producer qualification.
    let mut resolution = ResolutionOutput {
        root: ResolvedAncestor {
            requested_id: "tool:test/not-admitted".into(),
            resolved_ref: "tool:test/not-admitted".into(),
            source_path: "/diagnostic/not-admitted.yaml".into(),
            source_space: ItemSpace::Project,
            source_root: ItemSourceRoot::Project,
            trust_class: TrustClass::UntrustedProject,
            signer_fingerprint: None,
            alias_resolution: None,
            added_by: ResolutionStepName::PipelineInit,
            raw_content: "fixture".into(),
            source_content_digest: "a".repeat(64),
            raw_content_digest: "b".repeat(64),
        },
        ancestors: Vec::new(),
        references_edges: Vec::new(),
        referenced_items: Vec::new(),
        step_outputs: Default::default(),
        effective_trust_class: TrustClass::UntrustedProject,
        composed: KindComposedView {
            composed: json!({}),
            derived: Default::default(),
            policy_facts: Default::default(),
        },
    };
    let roots = fixture.state.engine.resolution_roots(None);
    let subject = SubjectResolutionAuthority::PinnedGeneration {
        snapshot_hash: "d".repeat(64),
    };
    let selection = ProductSelection {
        declaration_id: "runtime".into(),
        witness_hash: fixture.witness_hash.clone(),
        witness_source:
            ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
        qualification_hash: None,
    };
    for target in [
        ProductSelectionTarget::Root {},
        ProductSelectionTarget::ContentDependency {
            binding: "environment".into(),
        },
    ] {
        let inputs = vec![ProductSelectionInput {
            target: target.clone(),
            selection: selection.clone(),
        }];
        for recovered in [false, true] {
            let error = admit_root_product_selections(
                &fixture.state,
                REMOTE_ORIGIN,
                &fixture.state.engine,
                &roots,
                &subject,
                &mut resolution,
                Some(&context.fingerprint),
                Some(&context),
                &inputs,
                recovered,
            )
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("current site differs from the serving node"),
                "{target:?}, recovered={recovered}: {error:#}"
            );
            assert!(resolution.composed.derived.is_empty());
            if matches!(target, ProductSelectionTarget::ContentDependency { .. }) {
                // At the correct serving site this batch is left for the
                // separate content-dependency owner, not rejected for origin.
                admit_root_product_selections(
                    &fixture.state,
                    fixture.state.threads.site_id(),
                    &fixture.state.engine,
                    &roots,
                    &subject,
                    &mut resolution,
                    Some(&context.fingerprint),
                    Some(&context),
                    &inputs,
                    recovered,
                )
                .unwrap();
                assert!(resolution.composed.derived.is_empty());
            }
        }
    }
    let different_owner = format!("fp:{}", "f".repeat(64));
    let error = admit_root_product_selections(
        &fixture.state,
        fixture.state.threads.site_id(),
        &fixture.state.engine,
        &roots,
        &subject,
        &mut resolution,
        Some(&different_owner),
        Some(&context),
        &vec![ProductSelectionInput {
            target: ProductSelectionTarget::Root {},
            selection,
        }],
        false,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("ingress differs from its admitted owner")
    );
}

#[tokio::test]
async fn exact_import_refuses_unpublished_wrong_owner_and_wrong_node_witnesses() {
    let fixture = fixture(false, 4096, false);
    let observed = external_content_products::get(
        request(),
        fixture.context.clone(),
        Arc::clone(&fixture.state),
    )
    .await
    .unwrap();
    assert_eq!(observed["state"], "missing");
    let missing_producer = external_content_products::capture(
        request(),
        fixture.context.clone(),
        Arc::clone(&fixture.state),
    )
    .await
    .unwrap_err();
    assert!(format!("{missing_producer:#}").contains("producer terminal does not exist"));
    let unpublished = import(&fixture, &fixture.witness_hash, 11)
        .await
        .unwrap_err();
    assert!(format!("{unpublished:#}").contains("not currently published"));

    let (wrong_owner_hash, wrong_node_hash) = {
        let authority = fixture.state.state_store.pinned_state_authority().unwrap();
        let _guard = authority.acquire_shared_guard().unwrap();
        let cas = authority.cas_store().unwrap();
        let mut wrong_owner = fixture.evidence.clone();
        wrong_owner.owner_principal = format!("fp:{}", "f".repeat(64));
        let signer = NodeIdentitySigner::from_identity(&fixture.state.identity);
        let wrong_owner = wrong_owner
            .sign_attestation(&signer, "2026-09-07T00:00:00Z".to_owned())
            .unwrap();
        let other_node =
            NodeIdentity::create(&fixture._directory.path().join("other-node.pem")).unwrap();
        let foreign_signer = NodeIdentitySigner::from_identity(&other_node);
        let wrong_node = fixture
            .evidence
            .sign_attestation(&foreign_signer, "2026-09-07T00:00:00Z".to_owned())
            .unwrap();
        (
            cas.store_object(&wrong_owner.to_value()).unwrap(),
            cas.store_object(&wrong_node.to_value()).unwrap(),
        )
    };
    let wrong_owner = import(&fixture, &wrong_owner_hash, 11).await.unwrap_err();
    assert!(format!("{wrong_owner:#}").contains("not owned"));
    let wrong_node = import(&fixture, &wrong_node_hash, 11).await.unwrap_err();
    assert!(format!("{wrong_node:#}").contains("signature"));
}

#[tokio::test]
async fn malformed_subjects_fail_verification_and_cannot_acquire_import_authority() {
    for large_tier in [false, true] {
        let fixture = fixture(large_tier, 4096, false);
        let lying_witness = {
            let authority = fixture.state.state_store.pinned_state_authority().unwrap();
            let _guard = authority.acquire_shared_guard().unwrap();
            let cas = authority.cas_store().unwrap();
            let mut manifest = cas
                .get_object(&fixture.evidence.manifest_hash)
                .unwrap()
                .unwrap();
            // Internally consistent manifest accounting must not replace
            // verification of the retained blob's actual bytes.
            manifest["entries"][0]["size"] = json!(12);
            manifest["total_bytes"] = json!(12);
            let mut evidence = fixture.evidence.clone();
            evidence.manifest_hash = cas.store_object(&manifest).unwrap();
            evidence.total_bytes = 12;
            let signer = NodeIdentitySigner::from_identity(&fixture.state.identity);
            let attestation = evidence
                .sign_attestation(&signer, "2026-09-07T00:00:00Z".to_owned())
                .unwrap();
            cas.store_object(&attestation.to_value()).unwrap()
        };
        {
            let authority = fixture.state.state_store.pinned_state_authority().unwrap();
            let guard = authority.acquire_shared_guard().unwrap();
            let error = ryeos_state::external_content::products::publication::lookup_product_witness_hash_guarded(
                &authority,
                &ProductCaptureCoordinate::from_evidence(&fixture.evidence).unwrap(),
                &lying_witness,
                fixture.state.identity.verifying_key(),
                fixture.state.node_policy.require::<NodeObjectClosurePolicy>().unwrap().closure_limits().unwrap(),
                &guard,
            ).unwrap_err();
            assert!(format!("{error:#}").contains("size"), "{error:#}");
        }
        // Import requires current publication before scrubbing the payload.
        // An unattached attestation cannot bypass that gate, even when signed
        // by this node. The shared verifier above proves the size refusal.
        let error = import(&fixture, &lying_witness, 12).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("not currently published"),
            "{error:#}"
        );
    }
}

#[tokio::test]
async fn published_retry_survives_abandoned_upload_retirement() {
    let fixture = fixture(false, 4096, true);
    let staging_id = {
        let authority = fixture.state.state_store.pinned_state_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let operator = NodeIdentity::load(&fixture.state.config.operator_signing_key_path).unwrap();
        let mut stage = authority
            .require_recovery()
            .unwrap()
            .begin_durable_cas_upload_admitted(
                &guard,
                operator.fingerprint(),
                "external-content-import",
                &ryeos_state::DurableCasPublicationKey::external_content_import(&"e".repeat(64))
                    .unwrap(),
                None,
            )
            .unwrap();
        stage
            .protect_cas_closure(
                &guard,
                [fixture.evidence.manifest_hash.as_str()],
                std::iter::empty(),
            )
            .unwrap();
        // Equivalent durable state to interruption after product head publication
        // but before capture's old import stage was settled. The witness is the
        // authoritative answer; this unacknowledged stage is cleanup work only.
        stage.staging_id().to_owned()
    };
    let retry = external_content_products::capture(
        request(),
        fixture.context.clone(),
        Arc::clone(&fixture.state),
    )
    .await
    .unwrap();
    assert_eq!(retry["witness_hash"], fixture.witness_hash);
    assert_eq!(retry["idempotent"], true);
    {
        let authority = fixture.state.state_store.pinned_state_authority().unwrap();
        let guard = authority.acquire_exclusive_guard(true).unwrap();
        let recovery = authority.require_recovery().unwrap();
        // An explicit test maintenance cutoff, not a built-in product TTL.
        assert_eq!(
            recovery
                .retire_durable_cas_uploads_created_before("2099-01-01T00:00:00Z", &guard)
                .unwrap(),
            1,
        );
        let operator = NodeIdentity::load(&fixture.state.config.operator_signing_key_path).unwrap();
        assert!(
            recovery
                .open_durable_cas_upload_admitted(&guard, &staging_id, operator.fingerprint())
                .is_err()
        );
    }
    let observed = external_content_products::get(
        request(),
        fixture.context.clone(),
        Arc::clone(&fixture.state),
    )
    .await
    .unwrap();
    assert_eq!(observed["witness_hash"], fixture.witness_hash);
    assert_eq!(
        import(&fixture, &fixture.witness_hash, 11).await.unwrap()["manifest_hash"],
        fixture.evidence.manifest_hash,
    );
}
