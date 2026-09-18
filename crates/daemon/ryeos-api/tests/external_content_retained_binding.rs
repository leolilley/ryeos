//! Exact binding reuse keeps source testimony and destination admission separate.
mod test_state;

use std::sync::Arc;

use base64::Engine as _;
use ryeos_api::handlers::external_content_import;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::identity::{AuthorizedKeyPrincipalClass, NodeIdentity, WildcardPolicy};
use ryeos_app::node_policy::sections::external_content::{
    ExternalContentImportLimits, ExternalContentImportPolicyRecord, ManagedExternalContentPolicy,
};
use ryeos_app::operator_external_content::{ImportRequest, RetainedBindingImportRequest};
use ryeos_app::state::AppState;
use ryeos_state::objects::{ExternalContentBinding, ExternalContentConsumerAuthority};
use serde_json::json;

fn fixture(
    large_tier: bool,
) -> (
    tempfile::TempDir,
    Arc<AppState>,
    HandlerContext,
    String,
    String,
) {
    let (tmp, mut state) = test_state::build_test_state();
    let closure = state
        .node_policy
        .require::<ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy>()
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
                    max_file_bytes: 1024,
                    max_total_bytes: 4096,
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
    let operator = NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    let scopes = vec!["ryeos.execute.service.external-content/import".to_owned()];
    ryeos_app::identity::write_authorized_key_toml(
        &state.config.authorized_keys_dir,
        operator.fingerprint(),
        &base64::engine::general_purpose::STANDARD.encode(operator.verifying_key().as_bytes()),
        &scopes,
        "test operator",
        state.identity.fingerprint(),
        "2026-09-07T00:00:00Z",
        state.identity.signing_key(),
        WildcardPolicy::Reject,
    )
    .unwrap();
    let context = HandlerContext::new_with_authority(
        operator.principal_id(),
        scopes,
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    );
    let authority = state.state_store.pinned_state_authority().unwrap();
    let guard = authority.acquire_exclusive_guard(true).unwrap();
    let cas = authority.cas_store().unwrap();
    let blob = cas.store_blob(b"exact input").unwrap();
    let kind = if large_tier {
        ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
    } else {
        ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND
    };
    let schema = if large_tier {
        ryeos_state::objects::EXTERNAL_LARGE_CONTENT_SCHEMA
    } else {
        ryeos_state::objects::EXTERNAL_CONTENT_TREE_SCHEMA
    };
    let manifest = cas
        .store_object(&json!({
            "kind":kind, "schema":schema,
            "entries":[{"path":"content", "kind":"file", "mode":420, "blob_hash":blob, "size":11}],
            "entry_count":1, "total_bytes":11,
        }))
        .unwrap();
    let grant = ryeos_app::identity::load_verified_authorized_key(
        operator.fingerprint(),
        &state.config.authorized_keys_dir,
        &state.identity,
    )
    .unwrap()
    .unwrap();
    let binding = ExternalContentBinding::active(
        manifest.clone(),
        kind.to_owned(),
        ExternalContentConsumerAuthority::installed_bundle(
            "tool:test/source".to_owned(),
            "a".repeat(64),
        )
        .unwrap(),
        state.identity.fingerprint().to_owned(),
        operator.fingerprint().to_owned(),
        grant.source_file_hash,
    )
    .unwrap();
    let binding_hash = cas.store_object(&binding.to_value().unwrap()).unwrap();
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    state
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
    (tmp, Arc::new(state), context, binding_hash, manifest)
}

fn request(hash: &str, maximum_bytes: u64) -> ImportRequest {
    request_with_owner(hash, maximum_bytes, None)
}

fn request_with_owner(
    hash: &str,
    maximum_bytes: u64,
    binding_owner_principal: Option<String>,
) -> ImportRequest {
    ImportRequest::RetainedBinding(RetainedBindingImportRequest {
        binding_hash: hash.to_owned(),
        maximum_bytes,
        binding_owner_principal,
    })
}

fn replace_binding_with_remote_owner(
    directory: &tempfile::TempDir,
    state: &Arc<AppState>,
    binding_hash: &str,
) -> (String, String) {
    let remote_origin = format!("site:{}", "c".repeat(64));
    let remote = NodeIdentity::create(&directory.path().join("remote-operator.pem")).unwrap();
    let scopes = vec!["ryeos.execute.service.external-content/activate".to_owned()];
    ryeos_app::identity::reconcile_authorized_key_toml_scopes(
        &state.config.authorized_keys_dir,
        remote.fingerprint(),
        &base64::engine::general_purpose::STANDARD.encode(remote.verifying_key().as_bytes()),
        &scopes,
        "remote binding owner",
        state.identity.fingerprint(),
        "2026-09-07T00:00:00Z",
        &state.identity,
        WildcardPolicy::Reject,
        false,
        Some(&remote_origin),
        false,
    )
    .unwrap();
    let grant = ryeos_app::identity::load_verified_authorized_key(
        remote.fingerprint(),
        &state.config.authorized_keys_dir,
        &state.identity,
    )
    .unwrap()
    .unwrap();
    let authority = state.state_store.pinned_state_authority().unwrap();
    let guard = authority.acquire_exclusive_guard(true).unwrap();
    let cas = authority.cas_store().unwrap();
    let previous =
        ExternalContentBinding::from_value(&cas.get_object(binding_hash).unwrap().unwrap())
            .unwrap();
    let binding = ExternalContentBinding::active(
        previous.manifest_hash,
        previous.manifest_kind,
        previous.consumer,
        previous.target_node_fingerprint,
        remote.fingerprint().to_owned(),
        grant.source_file_hash.clone(),
    )
    .unwrap();
    let replacement_hash = cas.store_object(&binding.to_value().unwrap()).unwrap();
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    state
        .state_store
        .with_state_db(|db| {
            db.advance_generic_head_ref(
                ryeos_state::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE,
                &binding.binding_subject_id,
                &replacement_hash,
                Some(binding_hash),
                &signer,
                &guard,
            )
        })
        .unwrap();
    (remote.principal_id(), replacement_hash)
}

fn publish_binding_for_consumer(
    state: &Arc<AppState>,
    source_binding_hash: &str,
    consumer: ExternalContentConsumerAuthority,
) -> String {
    let authority = state.state_store.pinned_state_authority().unwrap();
    let guard = authority.acquire_exclusive_guard(true).unwrap();
    let cas = authority.cas_store().unwrap();
    let source =
        ExternalContentBinding::from_value(&cas.get_object(source_binding_hash).unwrap().unwrap())
            .unwrap();
    let binding = ExternalContentBinding::active(
        source.manifest_hash,
        source.manifest_kind,
        consumer,
        source.target_node_fingerprint,
        source.authorized_by,
        source.authorizer_grant_digest,
    )
    .unwrap();
    let binding_hash = cas.store_object(&binding.to_value().unwrap()).unwrap();
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    state
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
    binding_hash
}

fn fixture_bundle_engine(
    bundle_root: &std::path::Path,
    publisher: &NodeIdentity,
) -> Arc<ryeos_engine::engine::Engine> {
    let mut trust = ryeos_engine::test_support::live_trust_store();
    trust.extend_from(&ryeos_engine::trust::TrustStore::from_signers(vec![
        ryeos_engine::trust::TrustedSigner {
            fingerprint: publisher.fingerprint().to_owned(),
            verifying_key: *publisher.verifying_key(),
            label: Some("f05 fixture publisher".to_owned()),
        },
    ]));
    let core = ryeos_engine::test_support::core_bundle_root();
    let standard = ryeos_engine::test_support::standard_bundle_root();
    let ui = ryeos_engine::test_support::workspace_root().join("bundles/ryeos-ui");
    let kinds = ryeos_engine::kind_registry::KindRegistry::load_base(
        &[
            core.join(".ai/node/engine/kinds"),
            standard.join(".ai/node/engine/kinds"),
        ],
        &trust,
    )
    .unwrap();
    let roots = vec![core, standard, ui, bundle_root.to_path_buf()];
    let registered = ["core", "standard", "ryeos-ui", "f05-fixture"]
        .into_iter()
        .zip(roots.iter().cloned())
        .map(
            |(name, canonical_root)| ryeos_engine::item_resolution::RegisteredBundleRoot {
                name: name.to_owned(),
                canonical_root,
            },
        )
        .collect();
    let (parsers, _) =
        ryeos_engine::parsers::ParserRegistry::load_base(&roots, &trust, &kinds).unwrap();
    let handlers = ryeos_engine::test_support::load_live_handler_registry();
    let dispatcher = ryeos_engine::parsers::ParserDispatcher::new(parsers, Arc::clone(&handlers));
    let composers =
        ryeos_engine::composers::ComposerRegistry::from_kinds(&kinds, &handlers).unwrap();
    Arc::new(
        ryeos_engine::engine::Engine::new(kinds, dispatcher, roots)
            .with_registered_bundle_roots(registered)
            .with_trust_store(trust.clone())
            .with_node_trust_store(trust)
            .with_composers(composers),
    )
}

fn write_managed_activation_fixture_bundle(
    bundle_root: &std::path::Path,
    publisher: &NodeIdentity,
    manifest_hash: &str,
) {
    let ai = bundle_root.join(".ai");
    std::fs::create_dir_all(ai.join("tools/test")).unwrap();
    let manifest = r#"name: f05-fixture
version: "0.1.0"
description: F05 managed activation authority fixture
provides_kinds: []
requires_kinds: [tool]
uses_kinds: []
"#;
    std::fs::write(
        ai.join("manifest.yaml"),
        lillux::signature::sign_content(manifest, publisher.signing_key(), "#", None),
    )
    .unwrap();
    let tool = format!(
        r#"category: test
name: source
version: "1.0.0"
description: Exact managed activation consumer fixture
executor_id: "@subprocess"
execution_protocol: "protocol:ryeos/core/opaque"
effects: live
filesystem_authority: captured_execution
network_authority: isolated
external_content:
  - id: content
    kind: tree
    mode: pinned
    digest: {manifest_hash}
    mount_root: project
    mount: content
config:
  command: "bin:ryeos-core-tools"
  args: ["--help"]
  input_data: "${{params_json}}"
  timeout_secs: 10
"#
    );
    std::fs::write(
        ai.join("tools/test/source.yaml"),
        lillux::signature::sign_content(&tool, publisher.signing_key(), "#", None),
    )
    .unwrap();
}

async fn bind_through_managed_activation_owner(
    bundle: &tempfile::TempDir,
    publisher: &NodeIdentity,
    manifest_value: &serde_json::Value,
) -> (
    tempfile::TempDir,
    Arc<AppState>,
    String,
    String,
    ryeos_engine::resolution::ResolutionOutput,
) {
    let manifest_hash = ryeos_state::objects::canonical_value_digest(manifest_value).unwrap();
    write_managed_activation_fixture_bundle(bundle.path(), publisher, &manifest_hash);
    let engine = fixture_bundle_engine(bundle.path(), publisher);
    let (state_dir, mut state) = test_state::build_test_state_with_engine(engine);
    let closure = state
        .node_policy
        .require::<ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy>()
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
                    max_file_bytes: 1024,
                    max_total_bytes: 4096,
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
    let operator = NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    let scopes = vec!["ryeos.execute.service.external-content/activate".to_owned()];
    ryeos_app::identity::write_authorized_key_toml(
        &state.config.authorized_keys_dir,
        operator.fingerprint(),
        &base64::engine::general_purpose::STANDARD.encode(operator.verifying_key().as_bytes()),
        &scopes,
        "managed activation fixture operator",
        state.identity.fingerprint(),
        "2026-09-18T00:00:00Z",
        state.identity.signing_key(),
        WildcardPolicy::Reject,
    )
    .unwrap();

    let request_digest = "f".repeat(64);
    let authority = state.state_store.pinned_state_authority().unwrap();
    let guard = authority.acquire_shared_guard().unwrap();
    let cas = authority.cas_store().unwrap();
    assert_eq!(cas.store_object(manifest_value).unwrap(), manifest_hash);
    let key =
        ryeos_state::DurableCasPublicationKey::external_content_import(&request_digest).unwrap();
    let mut stage = authority
        .require_recovery()
        .unwrap()
        .begin_durable_cas_upload_admitted(
            &guard,
            operator.fingerprint(),
            "external-content-import",
            &key,
            None,
        )
        .unwrap();
    stage
        .protect_cas_closure(&guard, [manifest_hash.as_str()], std::iter::empty())
        .unwrap();
    let staging_id = stage.staging_id().to_owned();
    drop(stage);
    drop(guard);

    let component = ryeos_app::managed_external_content::ManagedActivationComponent {
        id: "content".to_owned(),
        storage: ryeos_app::managed_external_content::ManagedComponentStorage::Content,
        shape:
            ryeos_app::managed_external_content::ManagedActivationComponentShape::WholeArchiveTree {
                source: "archive".to_owned(),
                prefix: "content".to_owned(),
                bounds: ryeos_app::managed_external_content::ManagedActivationComponentBounds {
                    maximum_entries: 16,
                    maximum_depth: 4,
                    maximum_file_bytes: 1024,
                    maximum_total_bytes: 4096,
                },
            },
    };
    let activation =
        ryeos_app::managed_external_content::ResolvedManagedExternalContentActivation {
            activation_ref: "config:test/activation".to_owned(),
            activation_program_digest: "1".repeat(64),
            publisher_fingerprint: publisher.fingerprint().to_owned(),
            document: ryeos_app::managed_external_content::ManagedExternalContentActivation {
                schema: ryeos_app::managed_external_content::MANAGED_ACTIVATION_SCHEMA.to_owned(),
                consumer_ref: "tool:test/source".to_owned(),
                sources: Vec::new(),
                components: vec![component.clone()],
            },
            components: vec![
                ryeos_app::managed_external_content::ResolvedManagedActivationComponent {
                    recipe: component,
                    expected_manifest_hash: manifest_hash.clone(),
                    expected_manifest_kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND
                        .to_owned(),
                    declaration_kind: ryeos_engine::external_content::ExternalContentKind::Tree,
                    capture_bounds:
                        ryeos_app::managed_external_content::ManagedActivationComponentBounds {
                            maximum_entries: 16,
                            maximum_depth: 4,
                            maximum_file_bytes: 1024,
                            maximum_total_bytes: 4096,
                        },
                    expected_file_sha256: None,
                },
            ],
        };
    let state = Arc::new(state);
    let binding = ryeos_app::operator_external_content::bind_managed_activation_component(
        Arc::clone(&state),
        operator.fingerprint().to_owned(),
        &activation,
        ryeos_app::operator_external_content::BindRequest {
            staging_id,
            request_digest,
            manifest_hash: manifest_hash.clone(),
            consumer_ref: "tool:test/source".to_owned(),
            consumer_kind: ryeos_app::operator_external_content::BindConsumerKind::InstalledBundle,
            project_snapshot_hash: None,
            project_path: None,
            product_selections: None,
            product_owner_principal: None,
        },
    )
    .await
    .unwrap();
    let resolution = state
        .engine
        .effective_resolution_output(ryeos_engine::engine::EffectiveItemRequest {
            item_ref: ryeos_engine::canonical_ref::CanonicalRef::parse("tool:test/source").unwrap(),
            expected_kind: Some("tool".to_owned()),
            project_root: None,
            subject_resolution_authority:
                ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
        })
        .unwrap();
    (
        state_dir,
        state,
        binding.binding_hash,
        manifest_hash,
        resolution,
    )
}

fn portable_policy() -> ryeos_engine::runtime_registry::LaunchContentExternalPolicy {
    ryeos_engine::runtime_registry::LaunchContentExternalPolicy {
        allowed_mount_roots: vec![ryeos_state::objects::ExternalContentMountRoot::Project],
        max_declarations: 1,
        large_content_max_total_bytes: None,
    }
}

#[tokio::test]
async fn managed_bundle_binding_survives_pinned_preview_but_cannot_substitute_project_composition()
{
    use ryeos_app::external_content_admission::{
        admit_portable_content_dependency_in_publication, preview_portable_content_dependency,
    };
    use ryeos_engine::contracts::{ItemSourceRoot, ItemSpace, SubjectResolutionAuthority};
    use ryeos_engine::resolution::TrustClass;

    let bundle = tempfile::tempdir().unwrap();
    let publisher = NodeIdentity::create(&bundle.path().join("publisher.pem")).unwrap();
    let manifest_value = json!({
        "kind": ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
        "schema": ryeos_state::objects::EXTERNAL_CONTENT_TREE_SCHEMA,
        "entries": [],
        "entry_count": 0,
        "total_bytes": 0,
    });
    let (_state_dir, state, installed_binding_hash, _manifest_hash, fixed_bundle) =
        bind_through_managed_activation_owner(&bundle, &publisher, &manifest_value).await;
    let policy = portable_policy();
    let generation = SubjectResolutionAuthority::PinnedGeneration {
        snapshot_hash: "d".repeat(64),
    };

    // Managed activation publishes this exact InstalledBundle authority. An
    // outer pinned execution does not reclassify an unchanged bundle-owned
    // declaration, so preview and launch admission must select that head.
    let preview =
        preview_portable_content_dependency(&state, &fixed_bundle, &policy, &generation).unwrap();
    assert!(preview.ready_for_admission);
    assert_eq!(
        preview.declarations[0].binding_digest.as_deref(),
        Some(installed_binding_hash.as_str())
    );
    let mut admitted_fixed = fixed_bundle.clone();
    let mut fixed_publication = None;
    admit_portable_content_dependency_in_publication(
        &state,
        &mut admitted_fixed,
        &policy,
        &generation,
        None,
        &mut fixed_publication,
    )
    .unwrap();
    assert!(fixed_publication.is_some());

    // The same bundle root becomes generation-owned once its effective
    // definition includes a project contributor. The installed binding is an
    // incompatible authority, not a fallback candidate.
    let mut composed = fixed_bundle;
    let mut project_contributor = composed.root.clone();
    project_contributor.requested_id = "project/relationship".into();
    project_contributor.resolved_ref = "config:project/relationship".into();
    project_contributor.source_space = ItemSpace::Project;
    project_contributor.source_root = ItemSourceRoot::Project;
    project_contributor.trust_class = TrustClass::TrustedProject;
    project_contributor.signer_fingerprint = Some("e".repeat(64));
    composed.ancestors.push(project_contributor);

    let missing =
        preview_portable_content_dependency(&state, &composed, &policy, &generation).unwrap();
    assert!(!missing.ready_for_admission);
    assert_eq!(missing.declarations[0].status, "missing_binding");
    assert!(missing.declarations[0].binding_digest.is_none());
    let error = admit_portable_content_dependency_in_publication(
        &state,
        &mut composed.clone(),
        &policy,
        &generation,
        None,
        &mut None,
    )
    .err()
    .expect("installed binding must not substitute for project-composed authority");
    assert!(error.downcast_ref::<ryeos_app::external_content_admission::ExternalContentBindingUnavailable>().is_some());

    let project_consumer = ExternalContentConsumerAuthority::pinned_project(
        composed.root.resolved_ref.clone(),
        composed.root.signer_fingerprint.clone().unwrap(),
        generation.operational_generation().unwrap().to_owned(),
        ryeos_engine::external_content::pre_external_realization_consumer_digest(&composed)
            .unwrap(),
        None,
    )
    .unwrap();
    let pinned_binding_hash =
        publish_binding_for_consumer(&state, &installed_binding_hash, project_consumer);
    let ready =
        preview_portable_content_dependency(&state, &composed, &policy, &generation).unwrap();
    assert!(ready.ready_for_admission);
    assert_eq!(
        ready.declarations[0].binding_digest.as_deref(),
        Some(pinned_binding_hash.as_str())
    );
    let mut admitted_composed = composed;
    let mut composed_publication = None;
    admit_portable_content_dependency_in_publication(
        &state,
        &mut admitted_composed,
        &policy,
        &generation,
        None,
        &mut composed_publication,
    )
    .unwrap();
    assert!(composed_publication.is_some());
}

#[test]
fn portable_content_uses_the_admitted_project_generation_for_preview_and_launch() {
    use ryeos_app::external_content_admission::{
        admit_portable_content_dependency_in_publication, preview_portable_content_dependency,
    };
    use ryeos_engine::contracts::{ItemSourceRoot, ItemSpace, SubjectResolutionAuthority};
    use ryeos_engine::resolution::{
        KindComposedView, ResolutionOutput, ResolutionStepName, ResolvedAncestor, TrustClass,
    };

    let (_tmp, state, _context, source_binding_hash, manifest_hash) = fixture(false);
    let resolution = ResolutionOutput {
        root: ResolvedAncestor {
            requested_id: "test/environment".into(),
            resolved_ref: "config:test/environment".into(),
            source_path: "/fixture/.ai/config/test/environment.yaml".into(),
            source_space: ItemSpace::Project,
            source_root: ItemSourceRoot::Project,
            trust_class: TrustClass::TrustedProject,
            signer_fingerprint: Some("a".repeat(64)),
            alias_resolution: None,
            added_by: ResolutionStepName::PipelineInit,
            raw_content: String::new(),
            source_content_digest: "b".repeat(64),
            raw_content_digest: "c".repeat(64),
        },
        ancestors: Vec::new(),
        references_edges: Vec::new(),
        referenced_items: Vec::new(),
        step_outputs: Default::default(),
        effective_trust_class: TrustClass::TrustedProject,
        composed: KindComposedView::identity(json!({
            "external_content": [{
                "id":"content", "kind":"tree", "mode":"pinned",
                "digest":manifest_hash, "mount_root":"project", "mount":"content"
            }]
        })),
    };
    let snapshot_hash = "d".repeat(64);
    let consumer = ExternalContentConsumerAuthority::pinned_project(
        resolution.root.resolved_ref.clone(),
        "a".repeat(64),
        snapshot_hash.clone(),
        ryeos_engine::external_content::pre_external_realization_consumer_digest(&resolution)
            .unwrap(),
        None,
    )
    .unwrap();
    let authority = state.state_store.pinned_state_authority().unwrap();
    let guard = authority.acquire_exclusive_guard(true).unwrap();
    let cas = authority.cas_store().unwrap();
    let source =
        ExternalContentBinding::from_value(&cas.get_object(&source_binding_hash).unwrap().unwrap())
            .unwrap();
    let binding = ExternalContentBinding::active(
        manifest_hash,
        source.manifest_kind,
        consumer,
        source.target_node_fingerprint,
        source.authorized_by,
        source.authorizer_grant_digest,
    )
    .unwrap();
    let binding_hash = cas.store_object(&binding.to_value().unwrap()).unwrap();
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    state
        .state_store
        .with_state_db(|db| {
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
    let policy = ryeos_engine::runtime_registry::LaunchContentExternalPolicy {
        allowed_mount_roots: vec![ryeos_state::objects::ExternalContentMountRoot::Project],
        max_declarations: 1,
        large_content_max_total_bytes: None,
    };
    // COW uses its current admitted generation, not its original base or a
    // mutable path. Both preview and launch must select the same exact binding.
    for subject in [
        SubjectResolutionAuthority::PinnedGeneration {
            snapshot_hash: snapshot_hash.clone(),
        },
        SubjectResolutionAuthority::CowWorkspace {
            base_snapshot_hash: "e".repeat(64),
            current_operational_generation: snapshot_hash,
        },
    ] {
        let preview =
            preview_portable_content_dependency(&state, &resolution, &policy, &subject).unwrap();
        assert!(preview.ready_for_admission);
        assert_eq!(
            preview.declarations[0].binding_digest.as_deref(),
            Some(binding_hash.as_str())
        );
        let mut admitted = resolution.clone();
        let mut publication = None;
        admit_portable_content_dependency_in_publication(
            &state,
            &mut admitted,
            &policy,
            &subject,
            None,
            &mut publication,
        )
        .unwrap();
        assert!(publication.is_some());
    }
    for subject in [
        SubjectResolutionAuthority::Projectless,
        SubjectResolutionAuthority::LiveFs,
    ] {
        assert!(
            preview_portable_content_dependency(&state, &resolution, &policy, &subject).is_err()
        );
        assert!(
            admit_portable_content_dependency_in_publication(
                &state,
                &mut resolution.clone(),
                &policy,
                &subject,
                None,
                &mut None,
            )
            .is_err()
        );
    }
    let other = SubjectResolutionAuthority::PinnedGeneration {
        snapshot_hash: "f".repeat(64),
    };
    assert!(
        !preview_portable_content_dependency(&state, &resolution, &policy, &other)
            .unwrap()
            .ready_for_admission
    );
    let error = admit_portable_content_dependency_in_publication(
        &state,
        &mut resolution.clone(),
        &policy,
        &other,
        None,
        &mut None,
    )
    .err()
    .expect("another snapshot must not borrow the admitted consumer binding");
    assert!(error.downcast_ref::<ryeos_app::external_content_admission::ExternalContentBindingUnavailable>().is_some());
}

#[tokio::test]
async fn exact_binding_reuse_preserves_manifest_and_independent_durable_stage() {
    for large_tier in [false, true] {
        let (_tmp, state, context, binding_hash, manifest_hash) = fixture(large_tier);
        let first = external_content_import::handle(
            request(&binding_hash, 11),
            context.clone(),
            Arc::clone(&state),
        )
        .await
        .unwrap();
        let second = external_content_import::handle(
            request(&binding_hash, 11),
            context.clone(),
            Arc::clone(&state),
        )
        .await
        .unwrap();
        assert_eq!(first["manifest_hash"], manifest_hash);
        assert_eq!(first["total_bytes"], 11);
        assert_eq!(first["request_digest"], second["request_digest"]);
        assert_ne!(first["staging_id"], second["staging_id"]);
        let authority = state.state_store.pinned_state_authority().unwrap();
        let cas = authority.cas_store().unwrap();
        let binding =
            ExternalContentBinding::from_value(&cas.get_object(&binding_hash).unwrap().unwrap())
                .unwrap();
        ryeos_app::operator_external_content::release(
            Arc::clone(&state),
            context.clone(),
            ryeos_app::operator_external_content::ReleaseRequest {
                binding_subject_id: binding.binding_subject_id,
            },
        )
        .await
        .unwrap();
        assert!(
            external_content_import::handle(
                request(&binding_hash, 11),
                context,
                Arc::clone(&state)
            )
            .await
            .is_err()
        );
        // Receipt is reloaded from durable recovery, not a retained in-memory
        // stage handle. Source release cannot turn its admitted bytes into a
        // destination grant, nor invalidate the separate completed import.
        let guard = authority.acquire_shared_guard().unwrap();
        let stage = authority
            .require_recovery()
            .unwrap()
            .open_durable_cas_upload_admitted(
                &guard,
                first["staging_id"].as_str().unwrap(),
                &binding.authorized_by,
            )
            .unwrap();
        stage.ensure_protects_object(&manifest_hash).unwrap();
        assert!(stage.admitted_target_hash().is_none());
        stage
            .ensure_publication_contract(
                &ryeos_state::DurableCasPublicationKey::external_content_import(
                    first["request_digest"].as_str().unwrap(),
                )
                .unwrap(),
                None,
            )
            .unwrap();
    }
}

#[tokio::test]
async fn local_operator_explicitly_imports_an_admitted_remote_owned_binding() {
    let (directory, state, context, original_hash, manifest_hash) = fixture(false);
    let (remote_principal, binding_hash) =
        replace_binding_with_remote_owner(&directory, &state, &original_hash);

    let absent_owner = external_content_import::handle(
        request(&binding_hash, 11),
        context.clone(),
        Arc::clone(&state),
    )
    .await
    .unwrap_err();
    assert!(format!("{absent_owner:#}").contains("declared binding operator"));

    let wrong_owner = external_content_import::handle(
        request_with_owner(&binding_hash, 11, Some(format!("fp:{}", "b".repeat(64)))),
        context.clone(),
        Arc::clone(&state),
    )
    .await
    .unwrap_err();
    assert!(format!("{wrong_owner:#}").contains("grant was revoked"));

    let imported = external_content_import::handle(
        request_with_owner(&binding_hash, 11, Some(remote_principal.clone())),
        context.clone(),
        Arc::clone(&state),
    )
    .await
    .unwrap();
    assert_eq!(imported["manifest_hash"], manifest_hash);

    let remote_identity =
        NodeIdentity::load(&directory.path().join("remote-operator.pem")).unwrap();
    ryeos_app::identity::reconcile_authorized_key_toml_scopes(
        &state.config.authorized_keys_dir,
        remote_identity.fingerprint(),
        &base64::engine::general_purpose::STANDARD
            .encode(remote_identity.verifying_key().as_bytes()),
        &[
            "ryeos.execute.service.external-content/activate".to_owned(),
            "ryeos.execute.service.external-content/product".to_owned(),
        ],
        "changed remote binding owner",
        state.identity.fingerprint(),
        "2026-09-07T00:00:01Z",
        &state.identity,
        WildcardPolicy::Reject,
        false,
        Some(&format!("site:{}", "c".repeat(64))),
        false,
    )
    .unwrap();
    let changed_grant = external_content_import::handle(
        request_with_owner(&binding_hash, 11, Some(remote_principal.clone())),
        context.clone(),
        Arc::clone(&state),
    )
    .await
    .unwrap_err();
    assert!(format!("{changed_grant:#}").contains("grant changed"));

    let mut remote_caller = context;
    remote_caller.fingerprint = remote_principal;
    remote_caller.authorized_key_class = Some(AuthorizedKeyPrincipalClass::RemoteOperator);
    remote_caller.authenticated_origin_site_id = Some(format!("site:{}", "c".repeat(64)));
    let error = external_content_import::handle(
        request_with_owner(&binding_hash, 11, Some(remote_caller.fingerprint.clone())),
        remote_caller,
        state,
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("local_client configured operator"));
}

#[tokio::test]
async fn binding_reuse_refuses_nonowner_remote_stale_grant_and_byte_overclaims() {
    let (_tmp, state, context, hash, _) = fixture(false);
    for limit in [0, 10, 4097] {
        assert!(
            external_content_import::handle(
                request(&hash, limit),
                context.clone(),
                Arc::clone(&state)
            )
            .await
            .is_err()
        );
    }
    let mut foreign = context.clone();
    foreign.fingerprint = format!("fp:{}", "b".repeat(64));
    assert!(
        external_content_import::handle(request(&hash, 11), foreign, Arc::clone(&state))
            .await
            .is_err()
    );
    let mut remote = context.clone();
    remote.authorized_key_class = Some(AuthorizedKeyPrincipalClass::RemoteOperator);
    remote.authenticated_origin_site_id = Some("site:other".to_owned());
    assert!(
        external_content_import::handle(request(&hash, 11), remote, Arc::clone(&state))
            .await
            .is_err()
    );
    assert!(
        external_content_import::handle(
            request(&"d".repeat(64), 11),
            context.clone(),
            Arc::clone(&state)
        )
        .await
        .is_err()
    );
    let operator = NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    ryeos_app::identity::write_authorized_key_toml(
        &state.config.authorized_keys_dir,
        operator.fingerprint(),
        &base64::engine::general_purpose::STANDARD.encode(operator.verifying_key().as_bytes()),
        &context.scopes,
        "changed grant",
        state.identity.fingerprint(),
        "2026-09-07T00:00:01Z",
        state.identity.signing_key(),
        WildcardPolicy::Reject,
    )
    .unwrap();
    let error = external_content_import::handle(request(&hash, 11), context, state)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("grant changed"));
}

#[tokio::test]
async fn binding_reuse_rejects_lying_payloads_in_both_manifest_tiers() {
    for large_tier in [false, true] {
        let (_tmp, state, context, hash, manifest_hash) = fixture(large_tier);
        let authority = state.state_store.pinned_state_authority().unwrap();
        let cas = authority.cas_store().unwrap();
        let previous =
            ExternalContentBinding::from_value(&cas.get_object(&hash).unwrap().unwrap()).unwrap();
        let guard = authority.acquire_exclusive_guard(true).unwrap();
        let mut manifest = cas.get_object(&manifest_hash).unwrap().unwrap();
        manifest["entries"][0]["size"] = json!(12);
        manifest["total_bytes"] = json!(12);
        let manifest = cas.store_object(&manifest).unwrap();
        let binding = ExternalContentBinding::active(
            manifest,
            previous.manifest_kind,
            previous.consumer,
            previous.target_node_fingerprint,
            previous.authorized_by,
            previous.authorizer_grant_digest,
        )
        .unwrap();
        let hash = cas.store_object(&binding.to_value().unwrap()).unwrap();
        let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
        state
            .state_store
            .with_state_db(|db| {
                db.advance_generic_head_ref(
                    ryeos_state::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE,
                    &binding.binding_subject_id,
                    &hash,
                    None,
                    &signer,
                    &guard,
                )
            })
            .unwrap();
        drop(guard);
        let error = external_content_import::handle(request(&hash, 12), context, state)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("size"));
    }
}
