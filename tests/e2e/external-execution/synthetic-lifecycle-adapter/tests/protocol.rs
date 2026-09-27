#![recursion_limit = "256"]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD};

use ryeos_external_execution::lifecycle_adapter::{
    LifecycleAdapterInvocation, run_lifecycle_adapter,
};
use ryeos_external_execution::transport::ExternalExecutionChannelTransport as _;
use ryeos_external_execution_contract::{
    AllocationReservation, BoundOccurrence, ExternalGuestInputProjection, GuestBaseSnapshotInput,
    GuestMountAccess, GuestMountInput, GuestMountKind, GuestMountRole, LIFECYCLE_ADAPTER_PROTOCOL,
    LIFECYCLE_BOOTSTRAP_FD_ENV, LIFECYCLE_CREDENTIAL_FD_ENV, LIFECYCLE_GUEST_PACKAGE_FD_ENV,
    LIFECYCLE_LAUNCHER_FD_ENV, LIFECYCLE_SETTINGS_FD_ENV, LIFECYCLE_SUPERVISOR_FD_ENV,
    LifecycleAdapterInspectionRequest, LifecycleAdapterInspectionResponse, LifecycleAdapterRequest,
    LifecycleAdapterResponse, LifecycleArtifactInspection, LifecycleArtifactRole,
    LifecycleCapability, LifecycleOperationCommon, MAX_LIFECYCLE_RESPONSE_BYTES,
    SupervisorActivationIntent, TerminationIntent, from_json_slice_strict,
};
use ryeos_state::external_execution::admission::{
    AdmittedExternalCandidateProgram, ExternalCandidateExecutionRoute,
    ExternalCandidateProcFilesystem, ExternalCandidateRequirement, ExternalCandidateRuntimeRecipe,
    PROTOCOL,
};
use ryeos_state::external_execution::transport::{
    EXTERNAL_CHANNEL_ROUTE_CONTRACT, ExternalControllerTransportContract,
    ExternalNetworkInputPolicy, ExternalNetworkInputSelection, ExternalSupervisorBootstrap,
    external_tls_root_bundle_digest,
};

#[path = "../../support/signed_bundle.rs"]
mod signed_bundle;

#[path = "../../support/admitted_worker_evidence.rs"]
mod admitted_worker_evidence;
#[path = "../../support/candidate_authoring.rs"]
mod candidate_authoring;
#[path = "../../support/independent_verifier_scenario.rs"]
mod independent_verifier_scenario;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[path = "routed_verifier/mod.rs"]
mod routed_verifier;

use candidate_authoring::{
    CandidateAuthoringBundle, authored_codex_hosted_sources,
    configure_real_codex_scripted_turn_profile, joined_external_candidate_requirement,
    real_codex_requirement, sign_candidate_qualification_inputs,
    signed_candidate_operation_sources, write_candidate_authoring_bundle,
};

/// Observe the signed Worker source before selecting its subject product. This
/// authoring preview cannot borrow an identity from D1 qualification or a
/// later capsule, so the final signed policy has no recursive dependency.
fn admit_external_worker_evidence(
    state: &ryeos_app::state::AppState,
) -> admitted_worker_evidence::AdmittedWorkerEvidence {
    let evidence =
        admitted_worker_evidence::admit_worker(state, "worker:test/external-candidate").unwrap();
    assert!(
        evidence
            .profile
            .external_candidate_requirement()
            .unwrap()
            .is_some(),
        "captured Worker entry did not compile to an external-candidate profile"
    );
    evidence
}

fn fixture_qualification_use(
    bundle: &CandidateAuthoringBundle,
    evidence: &admitted_worker_evidence::AdmittedWorkerEvidence,
    runtime: &ryeos_state::objects::ExternalLargeContentManifestObject,
    real_codex: bool,
) -> ryeos_state::external_execution::admission::ExternalCandidateQualificationUse {
    use ryeos_state::objects::{
        ExternalContentKind, ExternalContentMode, ExternalContentMountRoot,
        ExternalContentRealization, ExternalContentRealizationSet,
    };
    let expected_requirement = if real_codex {
        real_codex_requirement()
    } else {
        joined_external_candidate_requirement()
    };
    let requirement = evidence
        .profile
        .external_candidate_requirement()
        .unwrap()
        .expect("captured Worker profile has no external-candidate requirement");
    assert_eq!(requirement, expected_requirement);
    let realized = ExternalContentRealizationSet::new(vec![ExternalContentRealization {
        id: "provider-runtime".into(),
        kind: ExternalContentKind::Tree,
        mode: ExternalContentMode::Pinned,
        manifest_hash: bundle.provider_runtime_manifest_hash.clone(),
        entry_count: runtime.entry_count,
        total_bytes: runtime.total_bytes,
        mount_root: ExternalContentMountRoot::ExecutionRuntime,
        mount: "provider-runtime".into(),
    }])
    .unwrap();
    ryeos_state::external_execution::admission::ExternalCandidateQualificationUse::from_admitted_inputs(
        &requirement,
        &evidence.profile,
        &evidence.source,
        &realized,
        &[],
        &BTreeMap::new(),
    )
    .unwrap()
}

fn fixture_repository_source_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("external-execution fixture must remain inside this repository")
}

struct TestBundleGenerationLifeline;

impl ryeos_engine::isolation::IsolationGenerationLifeline for TestBundleGenerationLifeline {
    fn begin_operation(&self) -> Result<Box<dyn Send + Sync>, String> {
        Ok(Box::new(()))
    }

    fn ensure_current(&self) -> Result<(), String> {
        Ok(())
    }
}

#[test]
fn changed_hosted_worker_bundles_pass_signed_source_preflight() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("external-execution fixture must remain inside this repository");
    let trust_document = ryeos_engine::trust::PublisherTrustDoc::parse(
        &std::fs::read_to_string(repository.join("bundles/core/PUBLISHER_TRUST.toml")).unwrap(),
    )
    .unwrap();
    let public_fixture = trust_document.decode_verifying_key().unwrap();
    let config = tempfile::tempdir().unwrap();
    let trusted = config.path().join("keys/trusted");
    std::fs::create_dir_all(&trusted).unwrap();
    ryeos_engine::trust::pin_key(&public_fixture, "development-publisher", &trusted, None).unwrap();
    let dependencies = [
        repository.join("bundles/core"),
        repository.join("bundles/standard"),
    ];
    let isolation = Arc::new(ryeos_engine::isolation::IsolationRuntime::disabled_for_authoring());
    for bundle in ["codex", "opencode"] {
        let source = repository.join("bundles").join(bundle);
        ryeos_bundle::preflight::preflight_verify_bundle_report_in_context(
            &source,
            &dependencies,
            config.path(),
            isolation.clone(),
        )
        .unwrap_or_else(|error| panic!("signed {bundle} bundle failed preflight: {error:#}"));
    }
}

fn live_bundle_engine(
    candidate_authoring_bundle: PathBuf,
    identity: &ryeos_app::identity::NodeIdentity,
    isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
) -> ryeos_engine::engine::Engine {
    let mut trust = ryeos_engine::test_support::live_trust_store();
    trust.extend_from(&ryeos_engine::trust::TrustStore::from_signers(vec![
        ryeos_engine::trust::TrustedSigner {
            fingerprint: identity.fingerprint().to_owned(),
            verifying_key: *identity.verifying_key(),
            label: Some("external candidate E2E authoring bundle".into()),
        },
    ]));
    let core = ryeos_engine::test_support::core_bundle_root();
    let standard = ryeos_engine::test_support::standard_bundle_root();
    let kinds = ryeos_engine::kind_registry::KindRegistry::load_base(
        &[
            core.join(".ai/node/engine/kinds"),
            standard.join(".ai/node/engine/kinds"),
        ],
        &trust,
    )
    .unwrap();
    let roots = vec![core, standard, candidate_authoring_bundle];
    let tagged_roots = roots
        .iter()
        .cloned()
        .map(|root| (root, ryeos_engine::resolution::TrustClass::TrustedBundle))
        .collect::<Vec<_>>();
    let registered_roots: Vec<_> = ["core", "standard", "external-candidate-authoring"]
        .into_iter()
        .zip(roots.iter().cloned())
        .map(
            |(name, canonical_root)| ryeos_engine::item_resolution::RegisteredBundleRoot {
                name: name.to_owned(),
                canonical_root,
            },
        )
        .collect();
    let isolation = Arc::new((*isolation).clone().retain_registered_generation(
        Arc::new(TestBundleGenerationLifeline),
        trust.clone(),
        registered_roots.clone(),
    ));
    let (parsers, _) =
        ryeos_engine::parsers::ParserRegistry::load_base(&roots, &trust, &kinds).unwrap();
    let handlers = ryeos_engine::test_support::load_live_handler_registry();
    let dispatcher = ryeos_engine::parsers::ParserDispatcher::new(parsers, Arc::clone(&handlers));
    let composers =
        ryeos_engine::composers::ComposerRegistry::from_kinds(&kinds, &handlers).unwrap();
    let runtimes = ryeos_engine::runtime_registry::RuntimeRegistry::build_from_bundles(
        &tagged_roots,
        &trust,
        &kinds,
    )
    .unwrap();
    let protocols =
        ryeos_engine::protocols::ProtocolRegistry::load_base(&tagged_roots, &trust).unwrap();
    let launch_preparers = ryeos_engine::launch_preparers::LaunchPreparerRegistry::from_runtimes(
        &runtimes,
        &handlers,
        ryeos_engine::launch_preparers::LaunchPreparerRunner::from_isolation_runtime(
            Arc::clone(&isolation),
            &roots,
        )
        .unwrap(),
    )
    .unwrap();
    ryeos_engine::engine::Engine::new(kinds, dispatcher, roots)
        .with_trust_store(trust.clone())
        .with_node_trust_store(trust)
        .with_composers(composers)
        .with_runtimes(runtimes)
        .with_launch_preparers(launch_preparers)
        .with_protocols(protocols)
        .with_isolation_generation(isolation)
        .with_registered_bundle_roots(registered_roots)
}

fn composed_isolation_policy() -> ryeos_engine::isolation::IsolationPolicy {
    let mut policy = ryeos_engine::isolation::IsolationPolicy::disabled_for_authoring();
    policy.mode = ryeos_engine::isolation::IsolationMode::Enforce;
    policy.backend = Some(ryeos_isolation_protocol::IsolationBackendSelection {
        bundle: "core".into(),
        implementation: "linux-lillux".into(),
    });
    policy.filesystem.proc_filesystem =
        ryeos_isolation_protocol::IsolationProcFilesystem::PidNamespace;
    // Deterministic signed fixtures use the explicit trusted process-group
    // contract, not a claim of provisioned hard process-scope containment.
    policy.trusted_process_group_sessions = true;
    policy
}

fn hosted_controller_isolation_policy() -> ryeos_engine::isolation::IsolationPolicy {
    let mut policy = ryeos_engine::isolation::IsolationPolicy::disabled_for_authoring();
    // The hosted controller is the explicitly trusted/disposable session
    // lane. A signed provider that creates a new connector process group may
    // use this authority; enforced strict-group isolation must instead have a
    // retained Lillux process scope and is tested as a refusal below.
    policy.trusted_process_group_sessions = true;
    policy
}

fn install_live_engine(
    state: &mut ryeos_app::state::AppState,
    candidate_authoring_bundle: PathBuf,
    large_runtime: bool,
    isolation_policy: ryeos_engine::isolation::IsolationPolicy,
) {
    use ryeos_app::node_policy::sections::external_content::{
        ExternalContentImportLimits, ExternalContentImportPolicyRecord,
        ManagedExternalContentPolicy,
    };

    // Retained-product composition still uses ordinary bounded import staging.
    // This fixture grants no ambient import roots or online activation.
    let import_policy = ExternalContentImportPolicyRecord {
        schema: 1,
        roots: BTreeMap::new(),
        limits: ExternalContentImportLimits {
            max_depth: 8,
            max_entries: 16,
            max_file_bytes: if large_runtime {
                512 * 1024 * 1024
            } else {
                64 * 1024 * 1024
            },
            max_total_bytes: if large_runtime {
                1024 * 1024 * 1024
            } else {
                128 * 1024 * 1024
            },
            store_budget_bytes: if large_runtime {
                2 * 1024 * 1024 * 1024
            } else {
                256 * 1024 * 1024
            },
            minimum_free_bytes: 64 * 1024 * 1024,
        },
        managed_activation: ManagedExternalContentPolicy {
            enabled: false,
            limits: None,
        },
    };
    import_policy.validate().unwrap();
    let core = ryeos_engine::test_support::core_bundle_root();
    let standard = ryeos_engine::test_support::standard_bundle_root();
    let trust = ryeos_engine::test_support::live_trust_store();
    let isolation = ryeos_app::engine_init::load_test_execution_isolation(
        &state.config.app_root,
        &[core, standard, candidate_authoring_bundle.clone()],
        &trust,
        isolation_policy,
    )
    .unwrap();
    let engine = Arc::new(live_bundle_engine(
        candidate_authoring_bundle,
        &state.identity,
        Arc::clone(&isolation),
    ));
    state.isolation = isolation;
    let trust = ryeos_engine::test_support::live_trust_store();
    let kinds = ryeos_engine::kind_registry::KindRegistry::load_base(
        &[
            ryeos_engine::test_support::core_bundle_root().join(".ai/node/engine/kinds"),
            ryeos_engine::test_support::standard_bundle_root().join(".ai/node/engine/kinds"),
        ],
        &trust,
    )
    .unwrap();
    let kind_profiles = Arc::new(ryeos_app::kind_profiles::KindProfileRegistry::build(Some(
        &kinds,
    )));
    state.threads = Arc::new(
        ryeos_app::thread_lifecycle::ThreadLifecycleService::new_for_test_with_site_id(
            state.state_store.clone(),
            engine.clone(),
            kind_profiles.clone(),
            state.events.clone(),
            state.event_streams.clone(),
            "site:composed-test",
        )
        .unwrap(),
    );
    state.commands = Arc::new(ryeos_app::command_service::CommandService::new(
        state.state_store.clone(),
        kind_profiles,
        state.events.clone(),
    ));
    state.engine = engine;
    state.services = Arc::new(ryeos_api::registry::build_service_registry());
    state.service_descriptors = ryeos_api::handlers::ALL;
    state.node_policy = Arc::new(
        ryeos_app::node_policy::NodePolicySnapshot::from_test_records(vec![
            Arc::new(ryeos_engine::history_policy::ResolvedNodeThreadHistoryPolicy::test_policy()),
            Arc::new(import_policy),
            Arc::new(
                ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy {
                    schema: 1,
                    max_roots: 256,
                    max_objects: 32_768,
                    max_blobs: 32_768,
                    max_object_bytes: 32 * 1024 * 1024,
                    max_total_object_bytes: 64 * 1024 * 1024,
                    // The large runtime is retained by the large-object
                    // store, not transported as a single small-CAS blob.
                    max_blob_bytes: 128 * 1024 * 1024,
                    max_total_blob_bytes: if large_runtime {
                        640 * 1024 * 1024
                    } else {
                        128 * 1024 * 1024
                    },
                    max_response_bytes: if large_runtime {
                        1024 * 1024 * 1024
                    } else {
                        256 * 1024 * 1024
                    },
                    max_links_per_object: 100_000,
                    local_verification: None,
                },
            ),
        ]),
    );
}

#[test]
fn authored_codex_turn_profile_compiles_for_external_candidate() {
    let mut profile = serde_json::Value::Null;
    let mut sources = BTreeMap::new();
    configure_real_codex_scripted_turn_profile(
        fixture_repository_source_root(),
        &mut profile,
        &mut sources,
        "http://127.0.0.1:12345",
    );
    let bytes = lillux::canonical_json(&profile).unwrap();
    ryeos_engine::structured_session_profile::compile(bytes.as_bytes(), &sources).unwrap();
    assert!(profile["workload_client"].is_null());
    assert!(profile["credential_subject"].is_null());
}

#[test]
fn shipped_external_codex_profile_is_explicit_and_closed() {
    let sources = authored_codex_hosted_sources(fixture_repository_source_root());
    let bytes = sources.get("external-authoring.profile.json").unwrap();
    ryeos_engine::structured_session_profile::compile(bytes, &sources).unwrap();
    let profile: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(profile["external_candidate"]["schema"], 6);
    assert_eq!(
        profile["external_candidate"]["required_lifecycle_capabilities"],
        serde_json::json!(["exact_terminal_observation"])
    );
    assert_eq!(
        profile["external_candidate"]["execution_route"],
        "connector_only"
    );
    assert_eq!(
        profile["external_candidate"]["runtime_product_declaration_id"],
        "guest-runtime"
    );
    assert!(profile["workload_client"].is_null());
    assert!(profile["credential_subject"].is_object());
}

fn install_joined_runtime_files(runtime: &Path) {
    for directory in ["bin", "lib64", "usr/lib"] {
        std::fs::create_dir_all(runtime.join(directory)).unwrap();
    }
    for (source, destination) in [
        (
            env!("CARGO_BIN_EXE_ryeos-synthetic-external-candidate-runtime"),
            runtime.join("bin/candidate"),
        ),
        (
            env!("CARGO_BIN_EXE_ryeos-synthetic-external-provider-runtime"),
            runtime.join("bin/provider"),
        ),
        (
            "/usr/lib/ld-linux-x86-64.so.2",
            runtime.join("lib64/ld-linux-x86-64.so.2"),
        ),
        ("/usr/lib/libc.so.6", runtime.join("usr/lib/libc.so.6")),
        (
            "/usr/lib/libgcc_s.so.1",
            runtime.join("usr/lib/libgcc_s.so.1"),
        ),
    ] {
        std::fs::copy(source, &destination).unwrap();
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

fn store_project_file(
    cas: &lillux::CasStore,
    files: &mut BTreeMap<String, String>,
    path: &str,
    bytes: &[u8],
    mode: u32,
) {
    let blob_hash = cas.store_blob(bytes).unwrap();
    let file_hash = cas
        .store_object(
            &ryeos_state::objects::ProjectFile {
                blob_hash,
                size: bytes.len() as u64,
                normalized_mode: mode,
            }
            .to_value(),
        )
        .unwrap();
    files.insert(path.to_owned(), file_hash);
}

fn install_test_operator_grant(state: &ryeos_app::state::AppState) {
    let operator =
        ryeos_app::identity::NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    let scopes = vec!["*".to_owned()];
    ryeos_app::identity::write_authorized_key_toml(
        &state.config.authorized_keys_dir,
        operator.fingerprint(),
        &STANDARD.encode(operator.verifying_key().as_bytes()),
        &scopes,
        "joined external candidate test operator",
        state.identity.fingerprint(),
        "2026-09-22T00:00:00Z",
        state.identity.signing_key(),
        ryeos_app::identity::WildcardPolicy::AllowBootstrap,
    )
    .unwrap();
    ryeos_app::identity::load_verified_authorized_key(
        operator.fingerprint(),
        &state.config.authorized_keys_dir,
        &state.identity,
    )
    .unwrap()
    .unwrap();
}

fn install_active_test_credential_profile(state: &ryeos_app::state::AppState) {
    let operator =
        ryeos_app::identity::NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    let profile_id = "credential:fixture";
    let home_id = "external-candidate-fixture-home";
    let lock_owner = "fixture-credential-enrollment";
    let login_id = "fixture-login";
    ryeos_app::private_artifact_home::create(
        &state.config.runtime_state_dir(),
        home_id,
        &Default::default(),
    )
    .unwrap();
    state
        .state_store
        .create_credential_profile(ryeos_app::runtime_db::NewCredentialProfile {
            profile_id,
            owner_principal: &operator.principal_id(),
            home_id,
        })
        .unwrap();
    state
        .state_store
        .acquire_credential_profile(profile_id, &operator.principal_id(), lock_owner)
        .unwrap();
    let epoch = state
        .state_store
        .begin_credential_enrollment(
            profile_id,
            lock_owner,
            login_id,
            lillux::time::timestamp_millis() as i64 + 60_000,
        )
        .unwrap();
    state
        .state_store
        .complete_credential_enrollment(
            profile_id,
            lock_owner,
            login_id,
            epoch,
            &serde_json::json!({"account":"fixture"}),
        )
        .unwrap();
    state
        .state_store
        .release_credential_profile(profile_id, lock_owner)
        .unwrap();
}

fn install_external_runtime_content(
    state: &ryeos_app::state::AppState,
    runtime_root: &Path,
    manifest: &ryeos_state::objects::ExternalContentManifestObject,
) {
    use ryeos_state::objects::ExternalContentManifestEntryKind;

    let authority = state.state_store.pinned_state_authority().unwrap();
    let _guard = authority.acquire_shared_guard().unwrap();
    let cas = authority.cas_store().unwrap();
    for entry in &manifest.entries {
        if entry.kind != ExternalContentManifestEntryKind::File {
            continue;
        }
        let bytes = std::fs::read(runtime_root.join(&entry.path)).unwrap();
        assert_eq!(entry.size, Some(bytes.len() as u64));
        let blob_hash = cas.store_blob(&bytes).unwrap();
        assert_eq!(entry.blob_hash.as_deref(), Some(blob_hash.as_str()));
    }
    let manifest_hash = cas
        .store_object(&serde_json::to_value(manifest).unwrap())
        .unwrap();
    assert_eq!(
        manifest_hash,
        ryeos_state::external_content_manifest_digest(manifest).unwrap()
    );
}

/// Exercise the production large-content capture and object store for the
/// pinned Codex executable. The 32 MiB content tier must not be widened for
/// this fixture.
fn install_external_large_runtime_content(
    state: &ryeos_app::state::AppState,
    runtime: &lillux::PinnedDirectory,
) -> ryeos_state::objects::ExternalLargeContentManifestObject {
    struct Sink {
        cas: lillux::CasStore,
        large: ryeos_state::LargeObjectStore,
    }
    impl ryeos_state::ExternalLargeContentSink for Sink {
        fn store_large_file(
            &mut self,
            file: std::fs::File,
            identity: ryeos_state::PinnedLargeObjectSourceIdentity,
            relative_path: &str,
            expected_sha256: Option<&str>,
        ) -> anyhow::Result<ryeos_state::IngestedLargeObject> {
            self.large
                .ingest_open_regular(file, identity, relative_path, expected_sha256)
        }

        fn store_content_file(
            &mut self,
            file: std::fs::File,
            _relative_path: &str,
            expected_size: u64,
        ) -> anyhow::Result<(String, u64)> {
            let bytes = lillux::read_open_regular_file_exact_bounded(
                file,
                expected_size,
                ryeos_state::objects::MAX_EXTERNAL_CONTENT_FILE_BYTES,
            )?;
            Ok((self.cas.store_blob(&bytes)?, expected_size))
        }
    }

    let authority = state.state_store.pinned_state_authority().unwrap();
    let _guard = authority.acquire_shared_guard().unwrap();
    let cas = authority.cas_store().unwrap();
    let large = authority.large_object_store().unwrap();
    let ignore =
        ryeos_state::ignore::IgnoreMatcher::from_config(&ryeos_state::ignore::IgnoreConfig {
            patterns: Vec::new(),
        })
        .unwrap();
    let policy = ryeos_state::LargeContentCapturePolicy::new(
        "runtime".into(),
        &ignore,
        ryeos_state::LargeContentCaptureBounds {
            max_depth: 8,
            max_entries: 16,
            max_file_bytes: 512 * 1024 * 1024,
            max_total_bytes: 1024 * 1024 * 1024,
        },
    )
    .unwrap();
    let mut sink = Sink { cas, large };
    let manifest = ryeos_state::capture_large_tree(runtime, &policy, &mut sink).unwrap();
    let manifest_hash = sink
        .cas
        .store_object(&manifest.to_value().unwrap())
        .unwrap();
    assert_eq!(
        manifest_hash,
        lillux::sha256_hex(
            lillux::canonical_json(&manifest.to_value().unwrap())
                .unwrap()
                .as_bytes(),
        )
    );
    manifest
}

fn publish_external_runtime_product_witness(
    state: &ryeos_app::state::AppState,
    manifest: &ryeos_state::objects::ExternalLargeContentManifestObject,
) -> String {
    use ryeos_engine::contracts::SubjectResolutionAuthority;
    use ryeos_state::external_content::products::publication::{
        ProductCaptureCoordinate, publish_product_witness,
    };
    use ryeos_state::external_content::products::{
        PRODUCT_CAPTURE_EVIDENCE_SCHEMA, PRODUCT_DECLARATIONS_SCHEMA, ProductCaptureEvidence,
        ProductDeclaration, ProductDeclarations, ProductProducerAdmission, ProductSource,
    };

    let relationship_resolution = state
        .engine
        .effective_resolution_output(ryeos_engine::engine::EffectiveItemRequest {
            item_ref: ryeos_engine::canonical_ref::CanonicalRef::parse(
                "config:fixtures/build_recipe",
            )
            .unwrap(),
            expected_kind: Some("config".into()),
            project_root: None,
            subject_resolution_authority: SubjectResolutionAuthority::Projectless,
        })
        .unwrap();
    let relationships =
        ryeos_state::external_content::products::composition::ProductRelationships::from_value(
            relationship_resolution.composed.composed["product_relationships"].clone(),
        )
        .unwrap();
    let relationship = relationships
        .select("auxiliary_to_verifier")
        .unwrap()
        .clone();
    let declaration = ProductDeclaration {
        name: relationship.producer.product_name.clone(),
        source: ProductSource::RetainedProject {},
        path: "products/external-runtime".into(),
        shape: relationship.required_product.shape,
        storage: relationship.required_product.storage,
        required: true,
        bounds: relationship.required_product.bounds.clone(),
        expected_manifest_hash: Some(lillux::sha256_hex(
            lillux::canonical_json(&manifest.to_value().unwrap())
                .unwrap()
                .as_bytes(),
        )),
    };
    let declarations = ProductDeclarations {
        schema: PRODUCT_DECLARATIONS_SCHEMA.into(),
        output_roots: Vec::new(),
        products: vec![declaration.clone()],
    };
    let producer = ProductProducerAdmission {
        canonical_ref: relationship.producer.canonical_ref.clone(),
        effective_definition_digest: "1".repeat(64),
        exact_program_hash: "2".repeat(64),
        producer_project_snapshot_hash: "3".repeat(64),
        launch_authority_digest: "4".repeat(64),
        admitted_parameters_digest: relationship.producer.admitted_parameters_digest().unwrap(),
    };
    let operator =
        ryeos_app::identity::NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    let manifest_hash = declaration.expected_manifest_hash.clone().unwrap();
    let evidence = ProductCaptureEvidence {
        schema: PRODUCT_CAPTURE_EVIDENCE_SCHEMA,
        owner_principal: operator.principal_id(),
        chain_root_id: "T-10000000-0000-0000-0000-000000000001".into(),
        thread_id: "T-10000000-0000-0000-0000-000000000002".into(),
        admitted_launch_capsule_hash: "5".repeat(64),
        producer: producer.clone(),
        root_producer: producer,
        result_project_snapshot_hash: "6".repeat(64),
        workspace_output_capture_hash: None,
        producer_partition_identity: None,
        recipe_binding: relationship.producer.recipe_binding.clone(),
        recipe_ref: relationship_resolution.root.resolved_ref.clone(),
        recipe_raw_content_digest: relationship_resolution.root.raw_content_digest.clone(),
        declarations_hash: declarations.content_hash().unwrap(),
        declarations,
        relationships,
        declaration,
        capture_policy_digest: "7".repeat(64),
        manifest_hash,
        manifest_kind: ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.into(),
        entry_count: manifest.entry_count,
        total_bytes: manifest.total_bytes,
    };
    let authority = state.state_store.pinned_state_authority().unwrap();
    let guard = authority.acquire_exclusive_guard(true).unwrap();
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    let attestation = evidence
        .sign_attestation(&signer, "2026-09-23T00:00:00Z".into())
        .unwrap();
    publish_product_witness(
        &authority,
        &ProductCaptureCoordinate::from_evidence(&evidence).unwrap(),
        &attestation,
        state
            .node_policy
            .require::<ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy>()
            .unwrap()
            .closure_limits()
            .unwrap(),
        &signer,
        &guard,
    )
    .unwrap()
    .witness
    .attestation_hash
}

fn bind_external_runtimes_to_qualification_verifier(
    state: &ryeos_app::state::AppState,
    manifest_hashes: &[&str],
) {
    use ryeos_engine::contracts::SubjectResolutionAuthority;

    let canonical =
        ryeos_engine::canonical_ref::CanonicalRef::parse("tool:fixtures/verify-external-runtime")
            .unwrap();
    let roots = state.engine.resolution_roots(None);
    let mut resolution = state
        .engine
        .effective_resolution_output(ryeos_engine::engine::EffectiveItemRequest {
            item_ref: canonical,
            expected_kind: Some("tool".into()),
            project_root: None,
            subject_resolution_authority: SubjectResolutionAuthority::Projectless,
        })
        .unwrap();
    let captured = ryeos_app::source_closure_admission::admit_source_closure(
        state,
        &state.engine,
        "tool",
        &mut resolution,
        &roots,
        None,
        None,
    )
    .unwrap();
    if let Some(captured) = captured
        && let Some(publication) = captured.into_publication()
    {
        publication.publish().unwrap();
    }
    let consumer = ryeos_app::external_content_admission::derive_consumer_authority_for_test(
        &resolution,
        &SubjectResolutionAuthority::Projectless,
    )
    .unwrap();
    let operator =
        ryeos_app::identity::NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    let operator_grant = ryeos_app::identity::load_verified_authorized_key(
        operator.fingerprint(),
        &state.config.authorized_keys_dir,
        &state.identity,
    )
    .unwrap()
    .expect("qualification fixture operator has no node-signed grant");
    let authority = state.state_store.pinned_state_authority().unwrap();
    let guard = authority.acquire_exclusive_guard(true).unwrap();
    let cas = authority.cas_store().unwrap();
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    // A synthetic subject and its independent verifier runtime may have the
    // same exact content identity. One active manifest/consumer/node binding
    // authorizes that content; attempting to author it twice is a conflicting
    // second head, not additional authority.
    for manifest_hash in manifest_hashes.iter().copied().collect::<BTreeSet<_>>() {
        let binding = ryeos_state::objects::ExternalContentBinding::active(
            manifest_hash.to_owned(),
            ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.into(),
            consumer.clone(),
            state.identity.fingerprint().to_owned(),
            operator.fingerprint().to_owned(),
            operator_grant.source_file_hash.clone(),
        )
        .unwrap();
        let binding_hash = cas.store_object(&binding.to_value().unwrap()).unwrap();
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
    }
}

async fn prepare_external_worker_program_and_binding(
    state: &Arc<ryeos_app::state::AppState>,
    engine: &Arc<ryeos_engine::engine::Engine>,
    project_root: &Path,
    subject: &ryeos_engine::contracts::SubjectResolutionAuthority,
    selection: &ryeos_state::external_content::products::composition::ProductSelection,
    requirement: ExternalCandidateRequirement,
    qualification_use: ryeos_state::external_execution::admission::ExternalCandidateQualificationUse,
) -> AdmittedExternalCandidateProgram {
    let roots = engine.resolution_roots(Some(project_root.to_path_buf()));
    let mut resolution = engine
        .effective_resolution_output(ryeos_engine::engine::EffectiveItemRequest {
            item_ref: ryeos_engine::canonical_ref::CanonicalRef::parse(
                "worker:test/external-candidate",
            )
            .unwrap(),
            expected_kind: Some("worker".into()),
            project_root: Some(project_root.to_path_buf()),
            subject_resolution_authority: subject.clone(),
        })
        .unwrap();
    let captured = ryeos_app::source_closure_admission::admit_source_closure(
        state,
        engine,
        "worker",
        &mut resolution,
        &roots,
        None,
        None,
    )
    .unwrap()
    .expect("external worker must admit its exact source closure");
    if let Some(publication) = captured.into_publication() {
        publication.publish().unwrap();
    }
    ryeos_app::effective_program_preparation::prepare_hookless_preselection_effective_program(
        engine,
        "worker",
        &mut resolution,
    )
    .unwrap();
    let pre_selection_digest =
        ryeos_engine::external_content::pre_product_selection_consumer_digest(&resolution).unwrap();
    let operator =
        ryeos_app::identity::NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    let context = ryeos_app::handler_context::HandlerContext::new_with_authority(
        operator.principal_id(),
        vec!["*".into()],
        true,
        Some(ryeos_app::identity::AuthorizedKeyPrincipalClass::LocalClient),
        None,
    );
    // Static qualification consumption must not dispatch another verifier.
    // Compare verified durable chain heads, not the thread-list projection.
    let chain_heads = || {
        let mut heads = state
            .state_store
            .with_state_db(|db| db.list_generic_head_refs("chains"))
            .unwrap()
            .into_iter()
            .map(|head| (head.namespace, head.name, head.target_hash))
            .collect::<Vec<_>>();
        heads.sort();
        heads
    };
    let chains_before_selection = chain_heads();
    let original_qualification = selection.qualification_hash.as_ref().map(|hash| {
        let authority = state.state_store.pinned_state_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let value = authority.cas_store().unwrap().get_object(hash).unwrap().unwrap();
        let attestation = ryeos_state::objects::Attestation::from_value(&value).unwrap();
        let evidence = ryeos_state::external_content::products::qualification::ProductQualificationEvidence::from_attestation(&attestation).unwrap();
        authority.ensure_guard(&guard).unwrap();
        evidence
    });
    if let Some(evidence) = &original_qualification {
        // Emit bounded identity-only failure evidence before selection. The
        // temporary node cleans up on panic; never print credentials or source.
        eprintln!(
            "qualification-consumption: {}",
            serde_json::json!({
                "qualification_hash": selection.qualification_hash,
                "product_witness_hash": selection.witness_hash,
                "consumer_isolation_enforced": state.isolation.is_enforced(),
                "verifier": evidence.verifier,
            })
        );
    }
    // Read-only identity oracle: preserve the original full selection and
    // qualification checks, but leave all binding publication to public compose.
    ryeos_app::operator_external_content::product_composition::select_products(
        state,
        &context,
        engine,
        &roots,
        subject,
        &mut resolution,
        std::slice::from_ref(selection),
    )
    .unwrap();
    assert_eq!(
        chain_heads(),
        chains_before_selection,
        "static qualification consumption changed an execution chain"
    );
    let selected =
        ryeos_engine::external_content::resolved_external_product_selections(&resolution)
            .unwrap()
            .expect("external worker selection did not produce retained product authority");
    let selected_qualification = selected
        .get(&selection.declaration_id)
        .unwrap()
        .qualification
        .as_ref();
    assert_eq!(
        selected_qualification.map(|proof| &proof.attestation_hash),
        selection.qualification_hash.as_ref()
    );
    assert_eq!(
        selected_qualification.map(|proof| &proof.evidence),
        original_qualification.as_ref(),
        "selection must retain the original verifier artifact and execution substrate"
    );
    let program = requirement
        .resolve_for_use(Some(&selected), &qualification_use)
        .unwrap();
    let consumer = ryeos_app::external_content_admission::derive_consumer_authority_for_test(
        &resolution,
        subject,
    )
    .unwrap();
    let selected_digest =
        ryeos_engine::external_content::pre_external_realization_consumer_digest(&resolution)
            .unwrap();
    let ryeos_engine::contracts::SubjectResolutionAuthority::PinnedGeneration { snapshot_hash } =
        subject
    else {
        panic!("external worker composition requires the exact pinned consumer generation")
    };
    assert!(
        matches!(
            &consumer,
            ryeos_state::objects::ExternalContentConsumerAuthority::PinnedProject {
                project_snapshot_hash, ..
            } if project_snapshot_hash == snapshot_hash
        ),
        "selected external worker consumer must retain the exact base generation"
    );
    let maximum_bytes = state.node_policy
        .require::<ryeos_app::node_policy::sections::external_content::ExternalContentImportPolicyRecord>()
        .unwrap().limits.max_total_bytes;
    let response = ryeos_api::handlers::external_content_products::compose(
        ryeos_app::operator_external_content::product_composition::ComposeRetainedProductsRequest {
            consumer_ref: "worker:test/external-candidate".into(),
            project_context: Some(ryeos_app::operator_external_content::product_composition::ProductCompositionProjectContext {
                snapshot_hash: snapshot_hash.clone(),
            }),
            selections: vec![selection.clone()],
            maximum_bytes,
        },
        context,
        Arc::clone(state),
    ).await.unwrap();
    assert_eq!(
        chain_heads(),
        chains_before_selection,
        "public composition changed an execution chain during static qualification consumption"
    );
    assert_eq!(
        response["consumer"],
        serde_json::to_value(&consumer).unwrap()
    );
    assert_eq!(
        response["project_context"],
        serde_json::json!({"snapshot_hash": snapshot_hash})
    );
    assert_eq!(response["selections"], serde_json::json!([selection]));
    assert_eq!(
        response["pre_selection_effective_definition_digest"],
        pre_selection_digest
    );
    assert_eq!(
        response["selected_effective_definition_digest"],
        selected_digest
    );
    assert_eq!(
        response["selection_identity_digests"],
        serde_json::to_value(BTreeMap::from([(
            selection.declaration_id.clone(),
            program.selection_identity_digest.clone()
        ),]))
        .unwrap()
    );
    let bindings = response["bindings"]
        .as_array()
        .expect("public composition omitted binding groups");
    assert_eq!(bindings.len(), 1);
    assert_eq!(
        bindings[0]["declaration_ids"],
        serde_json::json!([selection.declaration_id])
    );
    assert_eq!(bindings[0]["manifest_kind"], program.runtime_manifest_kind);
    assert_eq!(bindings[0]["manifest_hash"], program.runtime_manifest_hash);
    assert_eq!(
        bindings[0]["binding"]["consumer_ref"],
        "worker:test/external-candidate"
    );
    assert_eq!(
        bindings[0]["binding"]["manifest_hash"],
        program.runtime_manifest_hash
    );
    let binding_hash = bindings[0]["binding"]["binding_hash"]
        .as_str()
        .expect("public composition omitted binding hash");
    assert!(lillux::valid_hash(binding_hash));
    let authority = state.state_store.pinned_state_authority().unwrap();
    let guard = authority.acquire_shared_guard().unwrap();
    let binding_value = authority
        .cas_store()
        .unwrap()
        .get_object(binding_hash)
        .unwrap()
        .expect("public composition did not retain its binding");
    let binding = ryeos_state::objects::ExternalContentBinding::from_value(&binding_value).unwrap();
    assert_eq!(binding.consumer, consumer);
    assert_eq!(binding.manifest_hash, program.runtime_manifest_hash);
    assert_eq!(binding.manifest_kind, program.runtime_manifest_kind);
    assert_eq!(
        binding.target_node_fingerprint,
        state.identity.fingerprint()
    );
    assert_eq!(binding.authorized_by, operator.fingerprint());
    assert_eq!(
        binding.state,
        ryeos_state::objects::ExternalContentBindingState::Active
    );
    let head = state
        .state_store
        .with_state_db(|db| {
            db.read_generic_head_ref(
                ryeos_state::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE,
                &binding.binding_subject_id,
            )
        })
        .unwrap()
        .expect("public composition did not publish its binding head");
    assert_eq!(head.target_hash, binding_hash);
    authority.ensure_guard(&guard).unwrap();
    program
}

#[test]
fn signed_worker_admission_produces_real_external_candidate_capsule() {
    run_signed_worker_admission(SignedAdmissionCase::Ready);
}

#[test]
fn signed_worker_admission_refuses_unsupported_connector_process_group() {
    run_signed_worker_admission(SignedAdmissionCase::UnsupportedConnectorProcessGroup);
}

#[test]
fn signed_worker_preflight_refuses_missing_connector_before_allocation() {
    run_signed_worker_admission(SignedAdmissionCase::MissingConnector);
}

#[test]
fn signed_runtime_selection_refuses_missing_claim_before_allocation() {
    run_signed_worker_admission(SignedAdmissionCase::MissingRuntimeClaim);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SignedAdmissionCase {
    Ready,
    UnsupportedConnectorProcessGroup,
    MissingConnector,
    MissingRuntimeClaim,
}

fn run_signed_worker_admission(case: SignedAdmissionCase) {
    use ryeos_state::objects::{
        AdmittedPersistentSessionCapsule, ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree,
    };

    let root = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    lillux::PinnedDirectory::open(runtime.path())
        .unwrap()
        .unwrap()
        .tighten_owner_private_directory()
        .unwrap();
    install_joined_runtime_files(runtime.path());
    let runtime_directory = lillux::PinnedDirectory::open(runtime.path())
        .unwrap()
        .unwrap();
    let mut state = ryeos_app::state::test_support::build(root.path()).unwrap();
    install_test_operator_grant(&state);
    install_active_test_credential_profile(&state);
    let runtime_manifest = install_external_large_runtime_content(&state, &runtime_directory);
    let runtime_manifest_hash = lillux::sha256_hex(
        lillux::canonical_json(&runtime_manifest.to_value().unwrap())
            .unwrap()
            .as_bytes(),
    );
    state.started_at_iso = lillux::time::iso8601_now();
    let (external_bundle, external_trust, _) = install_signed_test_bundle(
        root.path(),
        case == SignedAdmissionCase::UnsupportedConnectorProcessGroup,
    );
    let external_artifacts = ryeos_app::external_artifacts::resolve_external_execution_artifacts(
        &[external_bundle],
        &external_trust,
    )
    .unwrap();
    state.external_candidate_connectors = Arc::new(external_artifacts.connectors);
    state.external_provider_configurations = Arc::new(external_artifacts.provider_configurations);
    state.external_placement_backends = Arc::new(external_artifacts.placement_backends);
    state
        .identity
        .write_public_identity(
            &state
                .config
                .runtime_root()
                .node()
                .join("identity/public-identity.json"),
        )
        .unwrap();
    std::fs::create_dir_all(state.config.runtime_root().trusted_keys_dir()).unwrap();
    let candidate_authoring_bundle = write_candidate_authoring_bundle(
        fixture_repository_source_root(),
        root.path(),
        &state.identity,
        &runtime_manifest_hash,
        &runtime_manifest_hash,
        false,
        true,
        None,
        None,
    );
    install_live_engine(
        &mut state,
        candidate_authoring_bundle.root.clone(),
        true,
        composed_isolation_policy(),
    );
    let admitted = admit_external_worker_evidence(&state);
    assert_eq!(
        admitted.profile, candidate_authoring_bundle.profile,
        "signed Worker capture compiled a different profile than authoring"
    );
    let qualification_use = fixture_qualification_use(
        &candidate_authoring_bundle,
        &admitted,
        &runtime_manifest,
        false,
    );
    sign_candidate_qualification_inputs(
        &candidate_authoring_bundle.root,
        &state.identity,
        &runtime_manifest_hash,
        &runtime_manifest_hash,
        &qualification_use.parameters().unwrap(),
        true,
    );
    install_live_engine(
        &mut state,
        candidate_authoring_bundle.root.clone(),
        true,
        composed_isolation_policy(),
    );
    let after = admit_external_worker_evidence(&state);
    assert_eq!(admitted.source, after.source);
    assert_eq!(admitted.profile, after.profile);
    let execution_identity = ryeos_app::execution_identity_probe::boot_node_execution_identity(
        &state.state_store,
        &state.identity,
        &state.daemon_build,
    )
    .unwrap();
    let mut extensions = (*state.extensions).clone();
    extensions.insert(execution_identity);
    state.extensions = Arc::new(extensions);
    let authority = state.state_store.pinned_state_authority().unwrap();
    let guard = authority.acquire_shared_guard().unwrap();
    let cas = authority.cas_store().unwrap();
    let policy = ProjectSnapshotPolicy::new(
        ryeos_state::project_sync::ProjectSyncScope::FullProject,
        vec![],
        vec![],
        Default::default(),
    )
    .unwrap();
    let policy_hash = cas.store_object(&policy.to_value()).unwrap();
    let tree_hash = cas
        .store_object(
            &ProjectTree {
                files: BTreeMap::new(),
            }
            .to_value(),
        )
        .unwrap();
    let base_snapshot_hash = cas
        .store_object(
            &ProjectSnapshot {
                project_tree_hash: tree_hash,
                effective_policy_hash: policy_hash,
                parent_hashes: vec![],
                created_at: "2026-09-22T00:00:00Z".into(),
                message: None,
                source: "ordinary-worker-admission-test".into(),
            }
            .to_value(),
        )
        .unwrap();
    drop(guard);
    drop(authority);

    let selections = ryeos_state::external_execution::admission::test_support::qualified_external_candidate_selections(
        &runtime_manifest_hash,
        &joined_external_candidate_requirement(),
        &qualification_use,
    )
    .unwrap();
    let mut selection_map = selections.into_inner();
    let selected_runtime = selection_map.get_mut("auxiliary").unwrap();
    selected_runtime.manifest_kind =
        ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.into();
    selected_runtime.relationship.required_product.storage =
        ryeos_state::external_content::products::ProductStorage::LargeContent;
    selected_runtime
        .relationship
        .required_product
        .bounds
        .maximum_file_bytes = 512 * 1024 * 1024;
    selected_runtime
        .relationship
        .required_product
        .bounds
        .maximum_total_bytes = 1024 * 1024 * 1024;
    if case == SignedAdmissionCase::MissingRuntimeClaim {
        let qualification = selected_runtime.qualification.as_mut().unwrap();
        qualification.evidence.result.claims.pop();
        qualification.evidence.verifier.result_digest =
            qualification.evidence.result.digest().unwrap();
    }
    let selections = ryeos_state::external_content::products::composition::ResolvedExternalProductSelections::new(selection_map);
    if case == SignedAdmissionCase::MissingRuntimeClaim {
        let error = selections.unwrap_err();
        assert!(
            format!("{error:#}").contains("qualification claims"),
            "unexpected refusal: {error:#}"
        );
        assert_eq!(
            ryeos_app::external_placement::fence_external_candidates_after_controller_restart(
                &state
            )
            .unwrap(),
            0,
            "missing runtime qualification claim left an unsettled allocation"
        );
        return;
    }
    let selections = selections.unwrap();
    let admission = ryeos_executor::test_support::admit_worker_session_with_selected_products(
        &mut state,
        "worker_execution:test/external-candidate",
        &project,
        &base_snapshot_hash,
        selections,
        joined_external_candidate_requirement(),
        qualification_use.clone(),
    );
    if case == SignedAdmissionCase::UnsupportedConnectorProcessGroup {
        let error = admission.unwrap_err();
        assert!(
            format!("{error:#}").contains("trusted controller process-group session"),
            "unexpected refusal: {error:#}"
        );
        return;
    }
    let capsule_hash = admission.unwrap();
    let authority = state.state_store.pinned_state_authority().unwrap();
    let _guard = authority.acquire_shared_guard().unwrap();
    let value = authority
        .cas_store()
        .unwrap()
        .get_object(&capsule_hash)
        .unwrap()
        .unwrap();
    let capsule = AdmittedPersistentSessionCapsule::from_current_value(&value).unwrap();
    assert_eq!(capsule.content_hash().unwrap(), capsule_hash);
    assert!(capsule.source_binding_hash.is_some());
    assert!(capsule.structured_session_profile.is_some());
    let program = capsule.external_candidate.as_ref().unwrap();
    if case == SignedAdmissionCase::MissingConnector {
        state.external_candidate_connectors =
            Arc::new(ryeos_app::external_placement::ExternalCandidateConnectorRegistry::default());
        let error =
            ryeos_app::external_placement::preflight_external_candidate_program(&state, program)
                .unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("exact signed external candidate connector is not installed"),
            "unexpected refusal: {error:#}"
        );
        assert_eq!(
            ryeos_app::external_placement::fence_external_candidates_after_controller_restart(
                &state
            )
            .unwrap(),
            0,
            "missing connector left an unsettled allocation"
        );
        return;
    }
    assert_eq!(program.requirement.provider_declaration_id, "codex-hosted");
    assert_eq!(program.runtime_manifest_hash, runtime_manifest_hash);
    assert_eq!(
        capsule
            .retained_product_selections
            .as_ref()
            .unwrap()
            .get("auxiliary")
            .unwrap()
            .manifest_hash,
        runtime_manifest_hash
    );
    ryeos_app::external_placement::preflight_external_candidate_program(&state, program).unwrap();
}

async fn execute_recorded_service(
    state: &Arc<ryeos_app::state::AppState>,
    owner: &str,
    service_ref: &str,
    params: serde_json::Value,
) -> ryeos_executor::executor::ServiceExecutionResult {
    let result = try_execute_recorded_service(state, owner, service_ref, params).await;
    result.unwrap_or_else(|error| {
        let threads = state.state_store.list_threads(100).unwrap();
        let snapshots = threads
            .iter()
            .filter_map(|thread| {
                state
                    .state_store
                    .get_authoritative_root_thread_snapshot(&thread.thread_id)
                    .ok()
                    .flatten()
            })
            .collect::<Vec<_>>();
        panic!(
            "{service_ref} failed: {error:#}; debug={error:?}; threads={threads:#?}; snapshots={snapshots:#?}"
        )
    })
}

async fn try_execute_recorded_service(
    state: &Arc<ryeos_app::state::AppState>,
    owner: &str,
    service_ref: &str,
    params: serde_json::Value,
) -> anyhow::Result<ryeos_executor::executor::ServiceExecutionResult> {
    use ryeos_engine::contracts::{
        EffectivePrincipal, ExecutionHints, PlanContext, Principal, ProjectContext,
        SubjectResolutionAuthority,
    };
    use ryeos_executor::executor::{
        ExecutionContext, ExecutionMode, ServiceRecordingAuthoritySource, ServiceRecordingContext,
    };

    let scopes = vec!["*".to_owned()];
    let site = state.threads.site_id().to_owned();
    let context = ExecutionContext {
        principal_fingerprint: owner.to_owned(),
        caller_scopes: scopes.clone(),
        engine: state.engine.clone(),
        plan_ctx: PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: owner.to_owned(),
                scopes: scopes.clone(),
            }),
            project_context: ProjectContext::None,
            subject_resolution_authority: SubjectResolutionAuthority::Projectless,
            current_site_id: site.clone(),
            origin_site_id: site,
            execution_hints: ExecutionHints::default(),
            scheduled_fire: None,
            validate_only: false,
        },
        requested_call: None,
    };
    let verified = ryeos_executor::executor::resolve_and_verify(
        &context.engine,
        &context.plan_ctx,
        service_ref,
        Some("service"),
    )?;
    let handler_context = ryeos_app::handler_context::HandlerContext::new_with_authority(
        owner.to_owned(),
        scopes,
        true,
        Some(ryeos_app::identity::AuthorizedKeyPrincipalClass::LocalClient),
        None,
    );
    let result = ryeos_executor::executor::execute_service_verified(
        verified,
        service_ref,
        params,
        ExecutionMode::Live,
        &context,
        state,
        ServiceRecordingContext {
            authority_source: ServiceRecordingAuthoritySource::ExplicitProjectless,
            usage_subject: None,
            usage_subject_asserted_by: None,
        },
        None,
        None,
        Some(handler_context),
    )
    .await;
    let result = result?;
    anyhow::ensure!(
        result.recorded,
        "candidate workflow services must be recorded"
    );
    Ok(result)
}

fn dispatch_projectless_public_item(
    state: &ryeos_app::state::AppState,
    item_ref: &str,
    expected_kind: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let state = state.clone();
    let item_ref = item_ref.to_owned();
    let expected_kind = expected_kind.to_owned();
    std::thread::Builder::new()
        .name("projectless-public-dispatch".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(dispatch_projectless_public_item_inner(
                    &state,
                    &item_ref,
                    &expected_kind,
                    params,
                ))
        })
        .unwrap()
        .join()
        .unwrap()
}

async fn dispatch_projectless_public_item_inner(
    state: &ryeos_app::state::AppState,
    item_ref: &str,
    expected_kind: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    use ryeos_engine::contracts::{
        EffectivePrincipal, ExecutionHints, PlanContext, Principal, ProjectContext,
        SubjectResolutionAuthority,
    };

    let operator =
        ryeos_app::identity::NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    let principal = operator.principal_id();
    let scopes = vec!["*".to_owned()];
    let site = state.threads.site_id().to_owned();
    let (workspace, lifeline) = ryeos_app::temp_dir_guard::create_projectless_workspace(
        &state.config.runtime_root().cache(),
        &format!(
            "public-dispatch-{}",
            lillux::sha256_hex(item_ref.as_bytes())
        ),
    )
    .unwrap();
    let authority = ryeos_state::objects::ExecutionProjectAuthority::projectless(
        ryeos_state::objects::EnvironmentAuthority::None,
    )
    .unwrap();
    let provenance = ryeos_app::execution_provenance::ExecutionProvenance::root_projectless(
        workspace.clone(),
        state.engine.clone(),
        lifeline,
        authority,
    )
    .unwrap();
    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: principal.clone(),
            scopes: scopes.clone(),
        }),
        project_context: ProjectContext::None,
        subject_resolution_authority: SubjectResolutionAuthority::Projectless,
        current_site_id: site.clone(),
        origin_site_id: site,
        execution_hints: ExecutionHints::default(),
        scheduled_fire: None,
        validate_only: false,
    };
    let context = ryeos_executor::executor::ExecutionContext {
        principal_fingerprint: principal,
        caller_scopes: scopes,
        engine: state.engine.clone(),
        plan_ctx,
        requested_call: None,
    };
    let binding = ryeos_app::thread_lifecycle::AdmittedProjectBinding::from_provenance(
        &context.engine,
        &context.plan_ctx,
        &provenance,
    )
    .unwrap();
    let ref_bindings = BTreeMap::new();
    let product_selections = Vec::new();
    let preflight = ryeos_executor::dispatch::preflight_root_dispatch(
        item_ref,
        expected_kind,
        &params,
        &ref_bindings,
        &product_selections,
        None,
        None,
        &binding,
        &context,
        state,
        None,
    )
    .unwrap();
    let root_admission = preflight.root_admission.unwrap();
    ryeos_executor::dispatch::admit_launch_contract(
        preflight.root_dispatch_evidence.applicability(),
        &root_admission,
        &ref_bindings,
        &ryeos_state::objects::ExecutionLifecycleAuthority::REQUEST_SCOPED,
        &provenance,
        &context,
        state,
    )
    .await
    .unwrap();
    ryeos_executor::dispatch::dispatch(
        item_ref,
        &ryeos_executor::dispatch::DispatchRequest {
            launch_mode: "wait",
            target_site_id: None,
            validate_only: false,
            params,
            ref_bindings,
            product_selections,
            acting_principal: &context.principal_fingerprint,
            project_path: &workspace,
            provenance,
            lifecycle_authority: ryeos_state::objects::ExecutionLifecycleAuthority::REQUEST_SCOPED,
            launch_timings: None,
            original_root_kind: expected_kind,
            pre_minted_thread_id: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            previous_thread_id: None,
            root_admission: Some(root_admission),
            root_dispatch_evidence: Some(preflight.root_dispatch_evidence),
            parent_execution_context: None,
            effect_authority: None,
        },
        &context,
        state,
    )
    .await
    .unwrap()
}

fn dispatch_pinned_public_worker(
    state: Arc<ryeos_app::state::AppState>,
    original_project: PathBuf,
    effective_project: PathBuf,
    request_engine: Arc<ryeos_engine::engine::Engine>,
    project_lifeline: Arc<ryeos_app::temp_dir_guard::TempDirGuard>,
    project_materialization: ryeos_state::PinnedProjectMaterialization,
    base_snapshot_hash: String,
    ref_bindings: BTreeMap<String, String>,
    environment_manifest_hash: Option<String>,
    product_selections:
        ryeos_state::external_content::products::composition::ProductSelectionInputs,
    params: serde_json::Value,
) -> (String, Result<serde_json::Value, String>) {
    std::thread::Builder::new()
        .name("external-public-worker-dispatch".into())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let project_authority = ryeos_state::objects::ExecutionProjectAuthority::pinned(
                    "project:external-public-worker".into(),
                    Some(original_project.clone()),
                    base_snapshot_hash,
                    ryeos_state::objects::PinnedProjectRealization::Cow {
                        terminal_publication:
                            ryeos_state::objects::PinnedTerminalPublication::RetainResult,
                    },
                    ryeos_state::objects::EnvironmentAuthority::None,
                    Vec::new(),
                )
                .unwrap()
                .with_child_policy(ryeos_state::objects::ChildProjectAuthorityPolicy::Inherit)
                .unwrap();
                let provenance =
                    ryeos_app::execution_provenance::ExecutionProvenance::root_pushed_head(
                        original_project,
                        request_engine.clone(),
                        project_lifeline,
                        project_materialization,
                        project_authority,
                    )
                    .unwrap();
                let operator = ryeos_app::identity::NodeIdentity::load(
                    &state.config.operator_signing_key_path,
                )
                .unwrap();
                let principal = operator.principal_id();
                let scopes = vec!["*".to_owned()];
                let site = state.threads.site_id().to_owned();
                let plan_ctx = ryeos_engine::contracts::PlanContext {
                    requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
                        ryeos_engine::contracts::Principal {
                            fingerprint: principal.clone(),
                            scopes: scopes.clone(),
                        },
                    ),
                    project_context: ryeos_engine::contracts::ProjectContext::LocalPath {
                        path: effective_project.clone(),
                    },
                    subject_resolution_authority: provenance.subject_resolution_authority(),
                    current_site_id: site.clone(),
                    origin_site_id: site,
                    execution_hints: ryeos_engine::contracts::ExecutionHints::default(),
                    scheduled_fire: None,
                    validate_only: false,
                };
                let context = ryeos_executor::executor::ExecutionContext {
                    principal_fingerprint: principal.clone(),
                    caller_scopes: scopes.clone(),
                    engine: request_engine,
                    plan_ctx,
                    requested_call: None,
                };
                let handler_context =
                    ryeos_app::handler_context::HandlerContext::new_with_authority(
                        principal,
                        scopes,
                        true,
                        Some(ryeos_app::identity::AuthorizedKeyPrincipalClass::LocalClient),
                        None,
                    );
                let binding = ryeos_app::thread_lifecycle::AdmittedProjectBinding::from_provenance(
                    &context.engine,
                    &context.plan_ctx,
                    &provenance,
                )
                .unwrap();
                let preflight = ryeos_executor::dispatch::preflight_root_dispatch(
                    "worker_execution:test/external-candidate",
                    "worker_execution",
                    &params,
                    &ref_bindings,
                    &product_selections,
                    None,
                    None,
                    &binding,
                    &context,
                    &state,
                    None,
                )
                .unwrap();
                if let Some(manifest_hash) = &environment_manifest_hash {
                    // Provision the exact prepared environment before public
                    // admission, just as the operator would. This does not
                    // inject product selections or admit a session.
                    let materialization = ryeos_app::resolution_cache::ResolutionMaterializationBinding::admitted_for_test(
                        context.plan_ctx.subject_resolution_authority.clone(),
                        Some(effective_project.clone()),
                        provenance.subject_workspace_lifeline(),
                        provenance.pinned_materialization().cloned(),
                    )
                    .unwrap();
                    let generation = context.engine.registered_bundle_generation_fingerprint();
                    let prepared = ryeos_executor::dispatch::prepare_launch_contract_with_materialization(
                        preflight.root_dispatch_evidence.applicability(),
                        &preflight.requested_subject.resolved,
                        &ref_bindings,
                        &effective_project,
                        &context,
                        ryeos_executor::execution::launch_preparation::PreparedResolutionCacheContext {
                            cache: &state.resolution_cache,
                            materialization: &materialization,
                            generation_identity: &generation,
                            plan_context_identity: "external-public-environment-provisioning",
                        },
                    )
                    .unwrap()
                    .expect("public worker must prepare its managed launch");
                    ryeos_executor::test_support::bind_prepared_worker_environment(
                        &state,
                        &prepared,
                        &context.plan_ctx.subject_resolution_authority,
                        manifest_hash,
                    )
                    .unwrap();
                }
                let root_admission = preflight.root_admission.unwrap();
                let pre_minted_thread_id = ryeos_app::thread_lifecycle::new_thread_id();
                ryeos_executor::dispatch::admit_launch_contract(
                    preflight.root_dispatch_evidence.applicability(),
                    &root_admission,
                    &ref_bindings,
                    &ryeos_state::objects::ExecutionLifecycleAuthority::REQUEST_SCOPED,
                    &provenance,
                    &context,
                    &state,
                )
                .await
                .unwrap();
                let result = ryeos_executor::dispatch::dispatch_with_handler_context(
                    "worker_execution:test/external-candidate",
                    handler_context,
                    &ryeos_executor::dispatch::DispatchRequest {
                        launch_mode: "wait",
                        target_site_id: None,
                        validate_only: false,
                        params,
                        ref_bindings,
                        product_selections,
                        acting_principal: &context.principal_fingerprint,
                        project_path: &effective_project,
                        provenance,
                        lifecycle_authority:
                            ryeos_state::objects::ExecutionLifecycleAuthority::REQUEST_SCOPED,
                        launch_timings: None,
                        original_root_kind: "worker_execution",
                        pre_minted_thread_id: Some(pre_minted_thread_id.clone()),
                        usage_subject: None,
                        usage_subject_asserted_by: None,
                        previous_thread_id: None,
                        root_admission: Some(root_admission),
                        root_dispatch_evidence: Some(preflight.root_dispatch_evidence),
                        parent_execution_context: None,
                        effect_authority: None,
                    },
                    &context,
                    &state,
                )
                .await
                .map_err(|error| format!("{error:#}"));
                (pre_minted_thread_id, result)
            })
        })
        .unwrap()
        .join()
        .unwrap()
}

async fn wait_for_terminal_thread(
    state: &Arc<ryeos_app::state::AppState>,
    thread_id: &str,
) -> ryeos_state::objects::ThreadSnapshot {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let thread = state
            .state_store
            .get_thread(thread_id)
            .unwrap()
            .unwrap_or_else(|| panic!("thread {thread_id} disappeared"));
        if matches!(thread.status.as_str(), "completed" | "failed" | "cancelled") {
            let snapshot = state
                .state_store
                .get_authoritative_root_thread_snapshot(thread_id)
                .unwrap()
                .unwrap_or_else(|| panic!("thread {thread_id} has no authoritative snapshot"));
            assert_eq!(
                thread.status,
                ryeos_state::objects::ThreadStatus::Completed.as_str(),
                "candidate operation failed: error={:#?}; result={:#?}; detail={thread:#?}",
                snapshot.error,
                snapshot.result,
            );
            return snapshot;
        }
        assert!(Instant::now() < deadline, "thread {thread_id} timed out");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn production_tls_config() -> Arc<rustls::ServerConfig> {
    use ryeos_external_candidate_supervisor::test_support::{
        TEST_SERVER_DER_BASE64, TEST_SERVER_KEY_DER_BASE64,
    };

    let certificate =
        rustls::pki_types::CertificateDer::from(STANDARD.decode(TEST_SERVER_DER_BASE64).unwrap());
    let private_key =
        rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
            STANDARD.decode(TEST_SERVER_KEY_DER_BASE64).unwrap(),
        ));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Arc::new(
        rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certificate], private_key)
            .unwrap(),
    )
}

fn read_http_request(reader: &mut impl std::io::Read) -> anyhow::Result<Vec<u8>> {
    let mut request = Vec::new();
    let header_end = loop {
        anyhow::ensure!(request.len() <= 1024 * 1024, "request exceeded test bound");
        let mut chunk = [0_u8; 4096];
        let count = reader.read(&mut chunk)?;
        anyhow::ensure!(count > 0, "request ended before headers");
        request.extend_from_slice(&chunk[..count]);
        if let Some(position) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = std::str::from_utf8(&request[..header_end])?;
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse::<usize>())
        })
        .transpose()?
        .unwrap_or(0);
    anyhow::ensure!(
        content_length <= 1024 * 1024,
        "request body exceeded test bound"
    );
    while request.len().saturating_sub(header_end) < content_length {
        let mut chunk = [0_u8; 4096];
        let count = reader.read(&mut chunk)?;
        anyhow::ensure!(count > 0, "request ended before body");
        request.extend_from_slice(&chunk[..count]);
    }
    request.truncate(header_end + content_length);
    Ok(request)
}

fn split_https_request(request: &[u8]) -> anyhow::Result<(&str, &[u8])> {
    let header_end = request
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("request has no header terminator"))?
        + 4;
    let headers = std::str::from_utf8(&request[..header_end])?;
    let path = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| anyhow::anyhow!("request has no path"))?;
    Ok((path, &request[header_end..]))
}

fn https_json_response(status: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

const SCRIPTED_MODEL_REQUESTS: usize =
    ryeos_independent_runtime_verifier::scripted_provider::REQUEST_COUNT;
const CONTROLLER_CANARY: &[u8] = b"synthetic controller canary unchanged\n";
const CONTROLLER_CANARY_DENIAL: &str = "controller-canary-read-denied";

fn scripted_local_write_command(path: &Path) -> String {
    let escaped = path.to_str().unwrap().replace('\'', "'\\''");
    format!("printf '%s' 'unexpected local write' > '{escaped}'")
}

fn scripted_canary_read_command(path: &Path) -> String {
    let path = path.to_str().unwrap();
    assert!(
        path.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
    );
    // The synthetic canary is one newline-terminated line. A real open/read
    // succeeds only if that controller file is erroneously visible in guest.
    format!(
        "if IFS= read -r line < \"{path}\"; then printf \"%s\" \"$line\"; else printf \"%s\" {CONTROLLER_CANARY_DENIAL}; exit 73; fi"
    )
}

// These are secondary observations at the scripted model peer, not an
// app-server notification transcript or runtime-qualification evidence. The
// independent routed verifier must also apply routing_observation to its own
// exact Codex notification stream and check guest/candidate evidence.
fn verify_scripted_routing_results(
    requests: &[serde_json::Value],
    canary_bytes: &[u8],
) -> anyhow::Result<()> {
    ryeos_independent_runtime_verifier::scripted_provider::check_requests(
        requests,
        CONTROLLER_CANARY,
        canary_bytes,
        CONTROLLER_CANARY_DENIAL,
    )
}

#[test]
fn scripted_routing_evidence_requires_refusal_and_unchanged_controller() {
    let requests = vec![
        serde_json::json!({"input":[]}),
        serde_json::json!({"input":[{"type":"function_call_output","call_id":"forbidden-local-command","output":"unknown turn environment id `local`"}]}),
        serde_json::json!({"input":[{"type":"function_call_output","call_id":"guest-command","output":"/workspace\nripgrep"}]}),
        serde_json::json!({"input":[{"type":"custom_tool_call_output","call_id":"candidate-edit","output":"Success. candidate-strategy.txt"}]}),
        serde_json::json!({"input":[{"type":"function_call_output","call_id":"guest-controller-secret-read","output":"Process exited with code 73\nOutput:\ncontroller-canary-read-denied\n"}]}),
    ];
    verify_scripted_routing_results(&requests, CONTROLLER_CANARY).unwrap();
    assert!(verify_scripted_routing_results(&requests, b"changed").is_err());
    assert!(verify_scripted_routing_results(&requests[..3], CONTROLLER_CANARY).is_err());
    for request in 1..SCRIPTED_MODEL_REQUESTS {
        let mut missing = requests.clone();
        missing[request]["input"] = serde_json::json!([]);
        assert!(verify_scripted_routing_results(&missing, CONTROLLER_CANARY).is_err());
    }
    let mut accepted_local = requests.clone();
    accepted_local[1]["input"][0]["output"] = "Process exited with code 0".into();
    assert!(verify_scripted_routing_results(&accepted_local, CONTROLLER_CANARY).is_err());
    for invalid in [
        "Process exited with code 0\nOutput:\ncontroller-canary-read-denied\n",
        "Process exited with code 73\nOutput:\nother command failure\n",
        "Process running with session ID 1\nOutput:\nProcess exited with code 73\ncontroller-canary-read-denied\n",
        "Process exited with code 73\nProcess exited with code 0\nOutput:\ncontroller-canary-read-denied\n",
        "Process exited with code 73\nOutput:\ncontroller-canary-read-denied\nsynthetic controller canary unchanged\n",
    ] {
        let mut bad_read = requests.clone();
        bad_read[4]["input"][0]["output"] = invalid.into();
        assert!(verify_scripted_routing_results(&bad_read, CONTROLLER_CANARY).is_err());
    }
}

fn start_scripted_responses_fixture(
    controller_canary: &Path,
) -> (
    String,
    std::sync::mpsc::Sender<()>,
    std::thread::JoinHandle<Vec<serde_json::Value>>,
) {
    // Synthetic data outside B, guest inputs and provider home. This path is
    // deliberately offered to a forbidden local-environment write and then an
    // explicit guest-environment read, never a real controller credential.
    // Retain the directory until the turn ends.
    std::fs::write(controller_canary, CONTROLLER_CANARY).unwrap();
    let forbidden_command = scripted_local_write_command(controller_canary);
    // Shell builtins avoid adding a cat/runtime dependency. The input
    // redirection actually attempts to open the controller path in the guest;
    // successful access prints its content and fails the observation checks.
    let secret_read_command = scripted_canary_read_command(controller_canary);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    listener.set_nonblocking(true).unwrap();
    let (turn_ready, turn_started) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        turn_started
            .recv_timeout(Duration::from_secs(180))
            .expect("scripted turn did not start after fixture setup");
        let mut requests = Vec::new();
        for number in 0..SCRIPTED_MODEL_REQUESTS {
            let deadline = Instant::now() + Duration::from_secs(40);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "scripted model request {number} timed out"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("scripted model listener failed: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let request = read_http_request(&mut stream).unwrap();
            let (path, body) = split_https_request(&request).unwrap();
            assert_eq!(path, "/responses");
            let headers = std::str::from_utf8(
                &request[..request
                    .windows(4)
                    .position(|part| part == b"\r\n\r\n")
                    .unwrap()],
            )
            .unwrap();
            assert!(headers.starts_with("POST /responses HTTP/1.1"));
            assert!(!headers.to_ascii_lowercase().contains("authorization:"));
            let model_request: serde_json::Value = serde_json::from_slice(body).unwrap();
            requests.push(model_request);
            let response_body =
                ryeos_independent_runtime_verifier::scripted_provider::response_sse(
                    number,
                    &forbidden_command,
                    &secret_read_command,
                )
                .unwrap();
            let mut response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len(),
            )
            .into_bytes();
            response.extend_from_slice(&response_body);
            stream.write_all(&response).unwrap();
            stream.flush().unwrap();
        }
        requests
    });
    (origin, turn_ready, worker)
}

fn private_fixture_rollout_kinds(home: &Path) -> Vec<String> {
    fn visit(directory: &Path, depth: usize, kinds: &mut Vec<String>) {
        if depth > 6 || kinds.len() >= 64 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                visit(&path, depth + 1, kinds);
            } else if kind.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
                && std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() <= 2 * 1024 * 1024)
                && let Ok(bytes) = std::fs::read(&path)
            {
                for line in bytes.split(|byte| *byte == b'\n') {
                    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) {
                        let kind = value["type"].as_str().unwrap_or("unknown");
                        let payload = value["payload"]["type"].as_str().unwrap_or("unknown");
                        kinds.push(format!("{kind}/{payload}"));
                    }
                }
            }
        }
    }
    let mut kinds = Vec::new();
    visit(&home.join("sessions"), 0, &mut kinds);
    kinds
}

fn dispatch_production_controller_request(
    runtime: &tokio::runtime::Runtime,
    state: &Arc<ryeos_app::state::AppState>,
    request: &[u8],
) -> anyhow::Result<(
    Vec<u8>,
    Option<ryeos_state::external_execution::ExecutionChannelPayload>,
    String,
)> {
    let (path, body) = split_https_request(request)?;
    let (value, payload, placement) = match path {
        "/external-execution/channel/attach" => {
            let request: ryeos_api::handlers::external_execution_channel::Request =
                serde_json::from_slice(body)?;
            let placement = request.placement_thread_id.clone();
            ryeos_app::external_placement::authenticate_external_channel_bootstrap(
                state,
                &request.placement_thread_id,
                &request.occurrence_id,
                &request.bootstrap_capability,
            )?;
            let context = ryeos_api::handler_context::HandlerContext::new(
                request.principal_id(),
                Vec::new(),
                true,
            );
            (
                runtime.block_on(ryeos_api::handlers::external_execution_channel::handle(
                    request,
                    context,
                    state.clone(),
                ))?,
                None,
                placement,
            )
        }
        "/external-execution/channel/exchange" => {
            let request: ryeos_api::handlers::external_execution_channel::ExchangeRequest =
                serde_json::from_slice(body)?;
            let placement = request.placement_thread_id.clone();
            let wire = request.decode_frame()?;
            ryeos_app::external_placement::authenticate_external_channel_frame(
                state,
                &request.placement_thread_id,
                &request.occurrence_id,
                &wire,
            )?;
            let binding = state
                .state_store
                .external_execution_channel(&request.placement_thread_id)?;
            let verified =
                ryeos_state::external_execution::SignedExecutionFrame::decode_and_verify(
                    &wire,
                    &binding,
                    lillux::time::timestamp_millis(),
                )?;
            let payload = verified.frame().payload.clone();
            let context = ryeos_api::handler_context::HandlerContext::new(
                request.principal_id(),
                Vec::new(),
                true,
            );
            (
                runtime.block_on(
                    ryeos_api::handlers::external_execution_channel::handle_exchange(
                        request,
                        context,
                        state.clone(),
                    ),
                )?,
                Some(payload),
                placement,
            )
        }
        other => anyhow::bail!("unexpected production controller path {other}"),
    };
    Ok((
        lillux::canonical_json(&value)?.into_bytes(),
        payload,
        placement,
    ))
}

fn serve_production_controller_tls(
    listener: TcpListener,
    state: Arc<ryeos_app::state::AppState>,
    expected_requests: usize,
) -> std::thread::JoinHandle<usize> {
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let tls_config = production_tls_config();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut handled = 0;
        while handled < expected_requests {
            let deadline = Instant::now() + Duration::from_secs(10);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "controller request timed out");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("controller listener failed: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let connection = rustls::ServerConnection::new(tls_config.clone()).unwrap();
            let mut tls = rustls::StreamOwned::new(connection, stream);
            let request = read_http_request(&mut tls).unwrap();
            let result = dispatch_production_controller_request(&runtime, &state, &request)
                .map(|(body, _, _)| body);
            let response = match result {
                Ok(body) => https_json_response("200 OK", &body),
                Err(error) => https_json_response(
                    "500 Internal Server Error",
                    serde_json::json!({"error":format!("{error:#}")})
                        .to_string()
                        .as_bytes(),
                ),
            };
            tls.write_all(&response).unwrap();
            tls.flush().unwrap();
            handled += 1;
        }
        handled
    })
}

fn serve_candidate_controller_tls(
    listener: TcpListener,
    state: Arc<ryeos_app::state::AppState>,
    stop: Arc<AtomicBool>,
    evidence: Arc<Mutex<Vec<String>>>,
) -> std::thread::JoinHandle<usize> {
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let tls_config = production_tls_config();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // A real Codex turn may run between channel frames. Keep the fixture
        // listener alive until the explicit stop signal, with a bounded idle
        // deadline that resets after each authenticated exchange.
        let mut idle_deadline = Instant::now() + Duration::from_secs(120);
        let mut handled = 0;
        loop {
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    if Instant::now() >= idle_deadline {
                        evidence
                            .lock()
                            .unwrap()
                            .push(format!("controller timed out after {handled} requests"));
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("candidate controller listener failed: {error}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let connection = rustls::ServerConnection::new(tls_config.clone()).unwrap();
            let mut tls = rustls::StreamOwned::new(connection, stream);
            let request = read_http_request(&mut tls).unwrap();
            let result = dispatch_production_controller_request(&runtime, &state, &request)
                // Production start/termination own readiness, release and
                // completion. The TLS fixture only carries protocol bytes.
                .map(|(body, _, _)| body);
            let response = match result {
                Ok(body) => https_json_response("200 OK", &body),
                Err(error) => {
                    let diagnostic = format!("{error:#}");
                    evidence
                        .lock()
                        .unwrap()
                        .push(format!("request {} failed: {diagnostic}", handled + 1));
                    https_json_response(
                        "500 Internal Server Error",
                        serde_json::json!({"error":diagnostic})
                            .to_string()
                            .as_bytes(),
                    )
                }
            };
            tls.write_all(&response).unwrap();
            tls.flush().unwrap();
            handled += 1;
            idle_deadline = Instant::now() + Duration::from_secs(120);
        }
        handled
    })
}

fn executable(path: &str) -> lillux::InheritedDescriptorAuthority {
    lillux::secure_fs::open_pinned_regular_file_no_follow(std::path::Path::new(path))
        .unwrap()
        .inherited_descriptor_authority()
        .unwrap()
}

fn artifact(
    authority: &lillux::InheritedDescriptorAuthority,
) -> (String, u64, LifecycleArtifactInspection) {
    let observation = authority.regular_file_observation().unwrap();
    let digest = authority
        .digest_regular_file_stable_exact(&observation)
        .unwrap();
    let descriptor = authority.inherited_descriptor().unwrap();
    (
        digest.clone(),
        observation.size(),
        LifecycleArtifactInspection {
            descriptor,
            digest,
            bytes: observation.size(),
        },
    )
}

fn install_signed_test_bundle(
    root: &Path,
    connector_new_group: bool,
) -> (PathBuf, ryeos_engine::trust::TrustStore, PathBuf) {
    let key = lillux::crypto::SigningKey::from_bytes(&[61; 32]);
    signed_bundle::install_signed_test_bundle(
        root,
        connector_new_group,
        &key,
        &signed_bundle::SyntheticExternalArtifacts {
            adapter: Path::new(env!(
                "CARGO_BIN_EXE_ryeos-synthetic-external-lifecycle-adapter"
            )),
            supervisor: Path::new(env!(
                "CARGO_BIN_EXE_ryeos-synthetic-external-candidate-supervisor"
            )),
            launcher: Path::new(env!(
                "CARGO_BIN_EXE_ryeos-synthetic-external-candidate-launcher"
            )),
            connector: Path::new(env!(
                "CARGO_BIN_EXE_ryeos-synthetic-external-candidate-connector"
            )),
            configuration: Path::new(env!(
                "CARGO_BIN_EXE_ryeos-synthetic-codex-external-configuration"
            )),
        },
    )
}

fn invoke_operation(
    adapter: &lillux::InheritedDescriptorAuthority,
    supervisor: &lillux::InheritedDescriptorAuthority,
    launcher: &lillux::InheritedDescriptorAuthority,
    settings: &[u8],
    credential: &[u8],
    request: &LifecycleAdapterRequest,
    bootstrap: Option<&ExternalSupervisorBootstrap>,
    inherited: Vec<lillux::InheritedDescriptorAuthority>,
) -> Vec<u8> {
    try_invoke_operation(
        adapter,
        supervisor,
        launcher,
        settings,
        credential,
        request,
        bootstrap,
        inherited,
        lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(5)),
    )
    .unwrap()
}

#[allow(clippy::too_many_arguments)]
fn try_invoke_operation(
    adapter: &lillux::InheritedDescriptorAuthority,
    supervisor: &lillux::InheritedDescriptorAuthority,
    launcher: &lillux::InheritedDescriptorAuthority,
    settings: &[u8],
    credential: &[u8],
    request: &LifecycleAdapterRequest,
    bootstrap: Option<&ExternalSupervisorBootstrap>,
    mut inherited: Vec<lillux::InheritedDescriptorAuthority>,
    deadline: lillux::time::MonotonicDeadline,
) -> anyhow::Result<Vec<u8>> {
    let request_handle =
        lillux::sealed_memfd(c"synthetic-request", &request.canonical_bytes().unwrap()).unwrap();
    let settings_handle = lillux::sealed_memfd(c"synthetic-settings", settings).unwrap();
    let credential_handle = lillux::sealed_memfd(c"synthetic-credential", credential).unwrap();
    let mut environment = vec![
        (
            LIFECYCLE_SUPERVISOR_FD_ENV.into(),
            supervisor.inherited_descriptor().unwrap().to_string(),
        ),
        (
            LIFECYCLE_LAUNCHER_FD_ENV.into(),
            launcher.inherited_descriptor().unwrap().to_string(),
        ),
        (
            LIFECYCLE_SETTINGS_FD_ENV.into(),
            settings_handle.inherited_descriptor().unwrap().to_string(),
        ),
        (
            LIFECYCLE_CREDENTIAL_FD_ENV.into(),
            credential_handle
                .inherited_descriptor()
                .unwrap()
                .to_string(),
        ),
    ];
    inherited.extend([
        supervisor.clone(),
        launcher.clone(),
        settings_handle,
        credential_handle,
    ]);
    if let Some(bootstrap) = bootstrap {
        let handle = lillux::sealed_memfd(
            c"synthetic-bootstrap",
            &bootstrap.canonical_bytes().unwrap(),
        )
        .unwrap();
        environment.push((
            LIFECYCLE_BOOTSTRAP_FD_ENV.into(),
            handle.inherited_descriptor().unwrap().to_string(),
        ));
        inherited.push(handle);
    }
    if let LifecycleAdapterRequest::ActivateSupervisor { guest_package, .. } = request {
        environment.push((
            LIFECYCLE_GUEST_PACKAGE_FD_ENV.into(),
            guest_package.descriptor.to_string(),
        ));
    }
    let output = run_lifecycle_adapter(
        adapter,
        LifecycleAdapterInvocation::Operate,
        &request_handle,
        inherited,
        environment,
        deadline,
    )?;
    anyhow::ensure!(
        !output.deadline_exceeded,
        "fixture lifecycle operation exceeded its deadline"
    );
    Ok(output.bytes)
}

#[test]
fn exact_protocol_boundary_allocates_and_reconciles_one_occurrence() {
    let adapter = executable(env!(
        "CARGO_BIN_EXE_ryeos-synthetic-external-lifecycle-adapter"
    ));
    let supervisor = executable(env!(
        "CARGO_BIN_EXE_ryeos-synthetic-external-candidate-supervisor"
    ));
    let launcher = executable(env!(
        "CARGO_BIN_EXE_ryeos-synthetic-external-candidate-launcher"
    ));
    let (adapter_hash, _, _) = artifact(&adapter);
    let (_, _, supervisor_artifact) = artifact(&supervisor);
    let (_, _, launcher_artifact) = artifact(&launcher);
    let provider_spec_bytes = br#"{"schema":1,"operations":[]}"#;
    let provider_spec =
        lillux::sealed_memfd(c"synthetic-provider-spec", provider_spec_bytes).unwrap();
    let provider_spec_artifact = LifecycleArtifactInspection {
        descriptor: provider_spec.inherited_descriptor().unwrap(),
        digest: lillux::sha256_hex(provider_spec_bytes),
        bytes: provider_spec_bytes.len() as u64,
    };
    let settings_schema_digest = "1".repeat(64);
    let capabilities = BTreeSet::from([
        LifecycleCapability::ExactAllocationReconciliation,
        LifecycleCapability::AuthoritativeNoOccurrence,
        LifecycleCapability::ExactActivationReconciliation,
        LifecycleCapability::IdempotentTermination,
        LifecycleCapability::ExactTerminalObservation,
    ]);
    let inspection = LifecycleAdapterInspectionRequest {
        schema: 1,
        protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
        adapter_id: "synthetic-local".into(),
        adapter_artifact_hash: adapter_hash,
        settings_schema_digest: settings_schema_digest.clone(),
        target: lillux::platform::current_binary_target()
            .unwrap()
            .to_owned(),
        declared_capabilities: capabilities,
        provider_spec: provider_spec_artifact,
        artifacts: BTreeMap::from([
            (LifecycleArtifactRole::Supervisor, supervisor_artifact),
            (LifecycleArtifactRole::Launcher, launcher_artifact),
        ]),
    };
    let inspection_request = lillux::sealed_memfd(
        c"synthetic-inspection",
        &ryeos_external_execution_contract::canonical_json(&inspection).unwrap(),
    )
    .unwrap();
    let response = run_lifecycle_adapter(
        &adapter,
        LifecycleAdapterInvocation::Inspect,
        &inspection_request,
        vec![supervisor.clone(), launcher.clone(), provider_spec.clone()],
        Vec::new(),
        lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(5)),
    )
    .unwrap();
    assert!(!response.deadline_exceeded);
    let response: LifecycleAdapterInspectionResponse =
        from_json_slice_strict(&response.bytes, MAX_LIFECYCLE_RESPONSE_BYTES).unwrap();
    response.validate_for(&inspection).unwrap();

    let state = tempfile::tempdir().unwrap();
    lillux::PinnedDirectory::open(state.path())
        .unwrap()
        .unwrap()
        .tighten_owner_private_directory()
        .unwrap();
    let credential = b"synthetic-secret";
    let settings = serde_json::json!({
        "expected_credential_sha256": lillux::sha256_hex(credential),
        "maximum_copy_depth": 8,
        "maximum_copy_entries": 1000,
        "schema": 1,
        "startup_timeout_ms": 1000,
        "state_root": state.path(),
    });
    let settings = lillux::canonical_json(&settings).unwrap().into_bytes();
    let settings_digest = lillux::sha256_hex(&settings);
    let common = LifecycleOperationCommon {
        schema: 1,
        protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
        operation_id: "a".repeat(64),
        binding_hash: "b".repeat(64),
        settings_digest,
    };
    let reservation = AllocationReservation {
        placement_thread_id: "T-synthetic-protocol".into(),
        admitted_capsule_hash: "c".repeat(64),
        base_snapshot_hash: "d".repeat(64),
        request_digest: "a".repeat(64),
        maximum_lifetime_seconds: 60,
        contact_deadline_ms: 1,
    };
    let invoke = |request: &LifecycleAdapterRequest| {
        invoke_operation(
            &adapter,
            &supervisor,
            &launcher,
            &settings,
            credential,
            request,
            None,
            Vec::new(),
        )
    };
    let allocate = LifecycleAdapterRequest::Allocate {
        common: common.clone(),
        reservation: reservation.clone(),
    };
    let allocated: LifecycleAdapterResponse =
        from_json_slice_strict(&invoke(&allocate), MAX_LIFECYCLE_RESPONSE_BYTES).unwrap();
    allocated.validate_for(&allocate).unwrap();
    let LifecycleAdapterResponse::AllocationBound { occurrence_id, .. } = allocated else {
        panic!("synthetic allocation did not bind")
    };
    assert_eq!(occurrence_id, format!("occ-{}", "a".repeat(48)));

    let reconcile = LifecycleAdapterRequest::ReconcileAllocation {
        common,
        reservation,
    };
    let reconciled: LifecycleAdapterResponse =
        from_json_slice_strict(&invoke(&reconcile), MAX_LIFECYCLE_RESPONSE_BYTES).unwrap();
    reconciled.validate_for(&reconcile).unwrap();
    assert!(matches!(
        reconciled,
        LifecycleAdapterResponse::AllocationBound { occurrence_id: found, .. }
            if found == occurrence_id
    ));
}

#[test]
fn production_loader_resolves_the_fixture_only_from_signed_bundle_authority() {
    let root = tempfile::tempdir().unwrap();
    let (bundle, trust, adapter_path) = install_signed_test_bundle(root.path(), false);
    ryeos_app::external_artifacts::resolve_external_execution_artifacts(
        std::slice::from_ref(&bundle),
        &trust,
    )
    .expect("signed test lifecycle generation should resolve and self-inspect");

    std::fs::write(&adapter_path, b"substituted after signed publication").unwrap();
    let error = ryeos_app::external_artifacts::resolve_external_execution_artifacts(
        std::slice::from_ref(&bundle),
        &trust,
    )
    .expect_err("changed lifecycle bytes must not retain signed executable authority");
    let message = format!("{error:#}");
    assert!(
        message.contains("hash") && message.contains("mismatch"),
        "{message}"
    );
}

#[test]
fn composed_controller_fixture_reaches_the_real_authenticated_attachment() {
    use ryeos_state::objects::{ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree};

    let root = tempfile::tempdir().unwrap();
    let base = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    for path in [base.path(), runtime.path()] {
        lillux::PinnedDirectory::open(path)
            .unwrap()
            .unwrap()
            .tighten_owner_private_directory()
            .unwrap();
    }
    std::fs::create_dir(runtime.path().join("bin")).unwrap();
    std::fs::write(runtime.path().join("bin/codex"), b"fixture runtime").unwrap();
    std::fs::set_permissions(
        runtime.path().join("bin/codex"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let base_directory = lillux::PinnedDirectory::open(base.path()).unwrap().unwrap();
    let runtime_directory = lillux::PinnedDirectory::open(runtime.path())
        .unwrap()
        .unwrap();
    let runtime_manifest =
        ryeos_state::observe_external_content_tree_exact(&runtime_directory).unwrap();
    let runtime_manifest_hash =
        ryeos_state::external_content_manifest_digest(&runtime_manifest).unwrap();

    let mut state = ryeos_app::state::test_support::build(root.path()).unwrap();
    let (external_bundle, external_trust, _) = install_signed_test_bundle(root.path(), false);
    let external_artifacts = ryeos_app::external_artifacts::resolve_external_execution_artifacts(
        &[external_bundle],
        &external_trust,
    )
    .unwrap();
    state.external_candidate_connectors = Arc::new(external_artifacts.connectors);
    state.external_provider_configurations = Arc::new(external_artifacts.provider_configurations);
    state.external_placement_backends = Arc::new(external_artifacts.placement_backends);
    let authority = state.state_store.pinned_state_authority().unwrap();
    let guard = authority.acquire_shared_guard().unwrap();
    let cas = authority.cas_store().unwrap();
    let policy = ProjectSnapshotPolicy::new(
        ryeos_state::project_sync::ProjectSyncScope::FullProject,
        vec![],
        vec![],
        Default::default(),
    )
    .unwrap();
    let policy_hash = cas.store_object(&policy.to_value()).unwrap();
    let tree = ProjectTree {
        files: Default::default(),
    };
    let tree_hash = cas.store_object(&tree.to_value()).unwrap();
    let base_snapshot = ProjectSnapshot {
        project_tree_hash: tree_hash,
        effective_policy_hash: policy_hash,
        parent_hashes: vec![],
        created_at: "2026-09-21T00:00:00Z".into(),
        message: None,
        source: "external-controller-composed-test".into(),
    };
    let base_snapshot_hash = cas.store_object(&base_snapshot.to_value()).unwrap();
    drop(guard);
    drop(authority);

    let recipe = ExternalCandidateRuntimeRecipe {
        schema: 2,
        runtime_mount_destination: "/runtime".into(),
        executable_relative_path: "bin/codex".into(),
        argv0: "codex".into(),
        arguments: vec!["exec-server".into(), "--listen".into(), "stdio".into()],
        cwd: "/workspace".into(),
        environment: BTreeMap::new(),
        max_stdout_bytes: 1024 * 1024,
        max_stderr_bytes: 1024 * 1024,
        proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
        contain_process_group: false,
        nested_sandbox: true,
    };
    let requirement = ExternalCandidateRequirement {
        schema: 6,
        required_lifecycle_capabilities: Default::default(),
        protocol: PROTOCOL.into(),
        connector_protocol: ryeos_state::external_execution::admission::CONNECTOR_PROTOCOL.into(),
        execution_route: ExternalCandidateExecutionRoute::ConnectorOnly,
        provider_declaration_id: "codex-hosted".into(),
        provider_configuration_destination: "environments.toml".into(),
        runtime_product_declaration_id: "auxiliary".into(),
        runtime_recipe: recipe,
    };
    let capsule = ryeos_state::external_execution::admission::test_support::qualified_external_candidate_capsule(
        requirement,
        &runtime_manifest_hash,
    )
    .unwrap();
    let base_authority = base_directory.inherited_descriptor_authority().unwrap();
    let runtime_authority = runtime_directory.inherited_descriptor_authority().unwrap();
    let runtime_manifest_bytes =
        lillux::canonical_json(&serde_json::to_value(&runtime_manifest).unwrap()).unwrap();
    let runtime_manifest_authority = lillux::sealed_memfd(
        c"controller-fixture-runtime-manifest",
        runtime_manifest_bytes.as_bytes(),
    )
    .unwrap();
    let guest_inputs = ExternalGuestInputProjection {
        schema: ryeos_external_execution_contract::EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
        base_snapshot: GuestBaseSnapshotInput {
            descriptor: base_authority.inherited_descriptor().unwrap(),
            snapshot_hash: base_snapshot_hash.clone(),
            closure_digest: "5".repeat(64),
            object_count: 3,
            blob_count: 0,
            total_bytes: 0,
        },
        workspace_outputs: None,
        inputs: vec![GuestMountInput {
            role: GuestMountRole::Product,
            authority_id: "auxiliary".into(),
            descriptor: runtime_authority.inherited_descriptor().unwrap(),
            destination: "/runtime".into(),
            kind: GuestMountKind::Directory,
            access: GuestMountAccess::ReadOnly,
            normalized_mode: None,
            content_authority:
                ryeos_external_execution_contract::GuestMountContentAuthority::ProductManifest {
                    manifest_kind:
                        ryeos_external_execution_contract::GuestProductManifestKind::Content,
                    manifest_hash: runtime_manifest_hash,
                    manifest_descriptor: runtime_manifest_authority.inherited_descriptor().unwrap(),
                    manifest_bytes: runtime_manifest_bytes.len() as u64,
                },
            bytes: runtime_manifest.total_bytes,
        }],
        executable_search: vec!["/runtime/bin".into()],
        environment: BTreeMap::new(),
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let controller_port = listener.local_addr().unwrap().port();
    let roots =
        vec![ryeos_external_candidate_supervisor::test_support::TEST_CA_DER_BASE64.to_owned()];
    let controller = ExternalControllerTransportContract {
        schema: 2,
        https_origin: format!("https://localhost:{controller_port}"),
        route_contract: EXTERNAL_CHANNEL_ROUTE_CONTRACT.into(),
        tls_root_bundle_digest: external_tls_root_bundle_digest(&roots).unwrap(),
        connect_timeout_ms: 1_000,
        request_timeout_ms: 2_000,
        maximum_response_bytes: 64 * 1024,
        network_inputs: ExternalNetworkInputPolicy {
            resolver: ExternalNetworkInputSelection {
                source: "/etc/resolv.conf".into(),
                max_bytes: 64 * 1024,
            },
            hosts: ExternalNetworkInputSelection {
                source: "/etc/hosts".into(),
                max_bytes: 64 * 1024,
            },
        },
    };
    let bootstrap = ryeos_app::external_placement::test_support::prepare_bound_external_supervisor(
        &mut state,
        capsule,
        base_snapshot_hash,
        guest_inputs,
        controller,
        roots,
        lillux::sha256_hex(
            &std::fs::read(env!(
                "CARGO_BIN_EXE_ryeos-synthetic-external-candidate-supervisor"
            ))
            .unwrap(),
        ),
        lillux::sha256_hex(
            &std::fs::read(env!(
                "CARGO_BIN_EXE_ryeos-synthetic-external-candidate-launcher"
            ))
            .unwrap(),
        ),
    )
    .unwrap();
    let state = Arc::new(state);
    let startup_deadline = ryeos_app::external_placement::external_candidate_startup_deadline(
        &state,
        &bootstrap.placement_thread_id,
    )
    .unwrap();
    let controller = serve_production_controller_tls(listener, state.clone(), 3);
    let supervisor = lillux::crypto::SigningKey::from_bytes(&[53; 32]);
    let supervisor_public_key =
        ryeos_state::external_execution::encode_channel_public_key(&supervisor.verifying_key())
            .unwrap();
    let (binding, mut channel) =
        ryeos_external_candidate_supervisor::attach_external_execution_channel(
            &bootstrap,
            &supervisor,
            &ryeos_state::external_execution::transport::ExternalCapturedNetworkInputs::from_bytes(
                &bootstrap.controller.network_inputs,
                b"nameserver 127.0.0.1\n",
                b"127.0.0.1 localhost\n",
            )
            .unwrap(),
        )
        .unwrap();
    bootstrap
        .validate_attached_binding(&binding, &supervisor_public_key)
        .unwrap();
    let ready = ryeos_state::external_execution::SignedExecutionFrame::sign(
        ryeos_state::external_execution::ExecutionFrame {
            schema: 1,
            binding_digest: binding.digest().unwrap(),
            direction: ryeos_state::external_execution::ChannelDirection::SupervisorToOwner,
            sequence: 1,
            previous_frame_digest: None,
            acknowledged_peer_sequence: 0,
            payload: ryeos_state::external_execution::ExecutionChannelPayload::Ready {
                supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                base_snapshot_hash: binding.base_snapshot_hash.clone(),
            },
        },
        &binding,
        &supervisor,
    )
    .unwrap();
    let ready_wire = lillux::canonical_json(&serde_json::to_value(ready).unwrap())
        .unwrap()
        .into_bytes();
    let first_exchange = channel.exchange(&ready_wire).unwrap();
    assert!(first_exchange.incoming_new);
    let first_outbound = first_exchange
        .outbound_frames
        .iter()
        .map(|frame| {
            ryeos_state::external_execution::SignedExecutionFrame::decode_and_verify(
                &frame.canonical_wire,
                &binding,
                lillux::time::timestamp_millis(),
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    assert!(!first_outbound.iter().any(|frame| matches!(
        frame.frame().payload,
        ryeos_state::external_execution::ExecutionChannelPayload::Release
    )));
    assert!(
        ryeos_app::external_placement::test_support::admit_retained_ready_and_author_release(
            &state,
            &bootstrap.placement_thread_id,
            startup_deadline,
        )
        .unwrap()
    );
    let replayed = channel.exchange(&ready_wire).unwrap();
    assert!(!replayed.incoming_new);
    let replayed_outbound = replayed
        .outbound_frames
        .iter()
        .map(|frame| {
            ryeos_state::external_execution::SignedExecutionFrame::decode_and_verify(
                &frame.canonical_wire,
                &binding,
                lillux::time::timestamp_millis(),
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    assert!(replayed_outbound.iter().any(|frame| matches!(
        frame.frame().payload,
        ryeos_state::external_execution::ExecutionChannelPayload::Release
    )));
    assert_eq!(controller.join().unwrap(), 3);
    assert_eq!(
        state
            .state_store
            .external_execution_channel(&bootstrap.placement_thread_id)
            .unwrap(),
        binding
    );
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn run_production_tls_candidate_with_test_stack(
    codex: Option<(PathBuf, String)>,
    scripted_turn: bool,
    publication: PublicationScenario,
) {
    std::thread::Builder::new()
        .name("external-candidate-e2e".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(16 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(run_production_tls_candidate(
                codex,
                scripted_turn,
                publication,
            ));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn production_tls_supervisor_exports_and_imports_exact_candidate_c() {
    run_production_tls_candidate_with_test_stack(None, false, PublicationScenario::Publish);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn public_bounded_worker_dispatch_uses_production_tls_candidate_route() {
    run_production_tls_candidate_with_test_stack(None, false, PublicationScenario::PublicDispatch);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn production_tls_stale_base_refuses_candidate_publication() {
    run_production_tls_candidate_with_test_stack(None, false, PublicationScenario::StaleBase);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
#[ignore = "requires explicit RYEOS_TEST_PINNED_CODEX matching the authored activation digest; no model turn"]
fn pinned_codex_handshake_uses_authenticated_remote_tls_environment() {
    run_production_tls_candidate_with_test_stack(
        Some(pinned_codex_artifact()),
        false,
        PublicationScenario::Publish,
    );
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
#[ignore = "requires explicit RYEOS_TEST_PINNED_CODEX; credential-free scripted model turn"]
fn pinned_codex_turn_freezes_remote_candidate_and_evaluates_from_base() {
    run_production_tls_candidate_with_test_stack(
        Some(pinned_codex_artifact()),
        true,
        PublicationScenario::PublicDispatch,
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PublicationScenario {
    Publish,
    StaleBase,
    PublicDispatch,
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn pinned_codex_artifact() -> (PathBuf, String) {
    let path = PathBuf::from(
        std::env::var_os("RYEOS_TEST_PINNED_CODEX")
            .expect("select the exact pinned Codex artifact explicitly"),
    );
    let activation: serde_yaml::Value = serde_yaml::from_slice(include_bytes!(
        "../../../../../bundles/codex/.ai/config/codex/activation.yaml"
    ))
    .unwrap();
    let expected = activation["sources"]
        .as_sequence()
        .unwrap()
        .iter()
        .flat_map(|source| source["members"].as_sequence().unwrap())
        .find(|member| member["path"].as_str() == Some("bin/codex"))
        .unwrap()["sha256"]
        .as_str()
        .unwrap();
    assert!(path.is_absolute());
    assert_eq!(
        lillux::sha256_hex(&std::fs::read(&path).unwrap()),
        expected,
        "unqualified provider artifact must not execute"
    );
    (path, expected.to_owned())
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn install_pinned_codex_command_tools(
    state: &ryeos_app::state::AppState,
    codex: &Path,
    closed_guest_shell: bool,
) -> (tempfile::TempDir, String) {
    let tools = stage_pinned_codex_command_tools(codex, closed_guest_shell);
    let tools_directory = lillux::PinnedDirectory::open(tools.path())
        .unwrap()
        .unwrap();
    let manifest = ryeos_state::observe_external_content_tree_exact(&tools_directory).unwrap();
    let hash = ryeos_state::external_content_manifest_digest(&manifest).unwrap();
    if !closed_guest_shell {
        let default_environment: serde_yaml::Value = serde_yaml::from_slice(include_bytes!(
            "../../../../../bundles/codex/.ai/config/codex/environments/default.yaml"
        ))
        .unwrap();
        assert_eq!(
            hash,
            default_environment["external_content"][0]["digest"]
                .as_str()
                .unwrap(),
            "handshake command tools must realize the exact authored content"
        );
    }
    install_external_runtime_content(state, tools.path(), &manifest);
    (tools, hash)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn stage_pinned_codex_command_tools(codex: &Path, closed_guest_shell: bool) -> tempfile::TempDir {
    let package_root = codex
        .parent()
        .and_then(Path::parent)
        .expect("pinned Codex path must be <package>/bin/codex");
    let activation: serde_yaml::Value = serde_yaml::from_slice(include_bytes!(
        "../../../../../bundles/codex/.ai/config/codex/environment-activation.yaml"
    ))
    .unwrap();
    let members = activation["sources"][0]["members"].as_sequence().unwrap();
    let tools = tempfile::tempdir().unwrap();
    std::fs::create_dir(tools.path().join("bin")).unwrap();
    for (source, destination) in [
        ("codex-path/rg", "bin/rg"),
        ("codex-resources/zsh/bin/zsh", "bin/zsh"),
    ] {
        let member = members
            .iter()
            .find(|member| member["path"].as_str() == Some(source))
            .expect("authored command tool member is absent");
        let bytes = std::fs::read(package_root.join(source)).unwrap();
        assert_eq!(
            lillux::sha256_hex(&bytes),
            member["sha256"].as_str().unwrap(),
            "command tool differs from the authored Codex activation pin"
        );
        let target = tools.path().join(destination);
        std::fs::write(&target, bytes).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    if closed_guest_shell {
        let inputs = PathBuf::from(
            std::env::var_os("RYEOS_TEST_AUTHORING_SOURCE_INPUTS")
                .expect("closed guest shell test needs RYEOS_TEST_AUTHORING_SOURCE_INPUTS"),
        );
        let contract: serde_yaml::Value = serde_yaml::from_slice(include_bytes!(
            "../../../../../.ai/config/development/ryeos/authoring-environment-inputs.yaml"
        ))
        .unwrap();
        let input_identities = &contract["inputs"];
        let lib = tools.path().join("lib");
        std::fs::create_dir(&lib).unwrap();
        for (target, source) in contract["files"].as_mapping().unwrap() {
            let target = target.as_str().unwrap();
            if !target.starts_with("environment/lib/") {
                continue;
            }
            let source = source.as_str().unwrap();
            let path = inputs.join(source);
            assert!(
                std::fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_file(),
                "authoring source library must be an ordinary regular file"
            );
            let bytes = std::fs::read(&path).unwrap();
            assert_eq!(
                lillux::sha256_hex(&bytes),
                input_identities[source]["sha256"].as_str().unwrap(),
                "authoring source library differs from its selected identity"
            );
            let target = tools
                .path()
                .join(target.strip_prefix("environment/").unwrap());
            std::fs::write(&target, bytes).unwrap();
            let mode = input_identities[source]["mode"].as_u64().unwrap() as u32;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        for source in ["elf/bin/patchelf", "elf/lib/ld-linux-x86-64.so.2"] {
            let path = inputs.join(source);
            assert!(
                std::fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_file()
            );
            assert_eq!(
                lillux::sha256_hex(&std::fs::read(path).unwrap()),
                input_identities[source]["sha256"].as_str().unwrap(),
                "ELF relocation tool differs from its selected input identity"
            );
        }
        let loader = inputs.join("elf/lib/ld-linux-x86-64.so.2");
        let patcher = inputs.join("elf/bin/patchelf");
        let shell = tools.path().join("bin/zsh");
        let runtime_root = "/ryeos/realizations/authoring-tools";
        let relocate = |shell: &Path, interpreter_first: bool| {
            let rpath = vec![
                "--no-sort".to_owned(),
                "--set-rpath".to_owned(),
                format!("{runtime_root}/lib"),
                "--no-default-lib".to_owned(),
                shell.to_string_lossy().into_owned(),
            ];
            let interpreter = vec![
                "--no-sort".to_owned(),
                "--set-interpreter".to_owned(),
                format!("{runtime_root}/lib/ld-linux-x86-64.so.2"),
                shell.to_string_lossy().into_owned(),
            ];
            let operations = if interpreter_first {
                [interpreter, rpath]
            } else {
                [rpath, interpreter]
            };
            for arguments in operations {
                let result = lillux::run(lillux::SubprocessRequest {
                    cmd: loader.to_string_lossy().into_owned(),
                    argv0: None,
                    args: [
                        vec![
                            "--inhibit-cache".to_owned(),
                            "--library-path".to_owned(),
                            inputs.join("elf/lib").to_string_lossy().into_owned(),
                            patcher.to_string_lossy().into_owned(),
                        ],
                        arguments,
                    ]
                    .concat(),
                    cwd: None,
                    envs: vec![("LANG".into(), "C".into()), ("LC_ALL".into(), "C".into())],
                    stdin_data: None,
                    timeout: 30.0,
                    limits: None,
                    inherited_fds: Vec::new(),
                    inherited_fd_mappings: Vec::new(),
                    supervised_status: None,
                });
                assert!(
                    result.success,
                    "selected ELF relocation refused: {result:?}"
                );
            }
        };
        let historic = tempfile::tempdir().unwrap();
        let historic_shell = historic.path().join("zsh");
        std::fs::copy(&shell, &historic_shell).unwrap();
        relocate(&historic_shell, true);
        let selection: serde_json::Value = serde_json::from_slice(include_bytes!(
            "../../../authoring-environment/selection.json"
        ))
        .unwrap();
        assert_eq!(
            lillux::sha256_hex(&std::fs::read(&historic_shell).unwrap()),
            selection["shell_sha256"].as_str().unwrap(),
            "the historical shell pin must have exact reproducible provenance"
        );
        relocate(&shell, false);
        // This fixture signs its measured output independently. Do not label
        // current relocation output as the historical authoring product: its
        // recorded shell pin was made by the earlier interpreter-first producer.
        eprintln!(
            "credential-free closed guest shell sha256: {}",
            lillux::sha256_hex(&std::fs::read(&shell).unwrap())
        );
    }
    tools
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
async fn run_production_tls_candidate(
    codex: Option<(PathBuf, String)>,
    scripted_turn: bool,
    publication: PublicationScenario,
) {
    if publication == PublicationScenario::StaleBase {
        eprintln!("stale-base fixture: preparing exact B and external admission");
    }
    use ryeos_state::objects::{ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree};

    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    assert!(!scripted_turn || codex.is_some());
    let controller_canary_directory = scripted_turn.then(|| tempfile::tempdir().unwrap());
    let controller_canary = controller_canary_directory
        .as_ref()
        .map(|directory| directory.path().join("controller-canary"));
    let (scripted_model_origin, scripted_turn_ready, scripted_model_worker) = if scripted_turn {
        let (origin, ready, worker) =
            start_scripted_responses_fixture(controller_canary.as_deref().unwrap());
        (Some(origin), Some(ready), Some(worker))
    } else {
        (None, None, None)
    };
    ryeos_executor::execution::arm_private_materialization_copy_limit(if codex.is_some() {
        1024 * 1024 * 1024
    } else {
        64 * 1024 * 1024
    })
    .unwrap();

    let root = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let qualification_runtime = tempfile::tempdir().unwrap();
    let provider_state = tempfile::tempdir().unwrap();
    for path in [
        runtime.path(),
        qualification_runtime.path(),
        provider_state.path(),
    ] {
        lillux::PinnedDirectory::open(path)
            .unwrap()
            .unwrap()
            .tighten_owner_private_directory()
            .unwrap();
    }

    if let Some((codex, expected)) = &codex {
        std::fs::create_dir(runtime.path().join("bin")).unwrap();
        std::fs::copy(codex, runtime.path().join("bin/codex")).unwrap();
        assert_eq!(
            &lillux::sha256_hex(&std::fs::read(runtime.path().join("bin/codex")).unwrap()),
            expected,
            "copied provider bytes must still match the authored activation pin"
        );
        std::fs::set_permissions(
            runtime.path().join("bin/codex"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    } else {
        install_joined_runtime_files(runtime.path());
    }
    // Qualification executes from its own admitted realization. The subject
    // product remains the exact Codex or synthetic guest runtime under test;
    // it never doubles as the verifier's executable authority.
    install_joined_runtime_files(qualification_runtime.path());
    let runtime_directory = lillux::PinnedDirectory::open(runtime.path())
        .unwrap()
        .unwrap();
    let mut state = ryeos_app::state::test_support::build(root.path()).unwrap();
    install_test_operator_grant(&state);
    install_active_test_credential_profile(&state);
    let (project, project_lifeline) = ryeos_app::temp_dir_guard::create_runtime_workspace(
        &root.path().join(".ai/state/cache"),
        "W-external-admission-base",
    )
    .unwrap();
    let runtime_manifest = install_external_large_runtime_content(&state, &runtime_directory);
    let runtime_manifest_hash = lillux::sha256_hex(
        lillux::canonical_json(&runtime_manifest.to_value().unwrap())
            .unwrap()
            .as_bytes(),
    );
    let qualification_runtime_directory =
        lillux::PinnedDirectory::open(qualification_runtime.path())
            .unwrap()
            .unwrap();
    let qualification_runtime_manifest =
        install_external_large_runtime_content(&state, &qualification_runtime_directory);
    let qualification_runtime_manifest_hash = lillux::sha256_hex(
        lillux::canonical_json(&qualification_runtime_manifest.to_value().unwrap())
            .unwrap()
            .as_bytes(),
    );
    let command_tools = codex
        .as_ref()
        .map(|(path, _)| install_pinned_codex_command_tools(&state, path, scripted_turn));
    let command_tools_manifest_hash = command_tools.as_ref().map(|(_, hash)| hash.as_str());
    state.started_at_iso = lillux::time::iso8601_now();
    let (external_bundle, external_trust, _) =
        install_signed_test_bundle(root.path(), codex.is_some());
    let external_artifacts = ryeos_app::external_artifacts::resolve_external_execution_artifacts(
        &[external_bundle],
        &external_trust,
    )
    .unwrap();
    state.external_candidate_connectors = Arc::new(external_artifacts.connectors);
    state.external_provider_configurations = Arc::new(external_artifacts.provider_configurations);
    state.external_placement_backends = Arc::new(external_artifacts.placement_backends);
    state
        .identity
        .write_public_identity(
            &state
                .config
                .runtime_root()
                .node()
                .join("identity/public-identity.json"),
        )
        .unwrap();
    std::fs::create_dir_all(state.config.runtime_root().trusted_keys_dir()).unwrap();
    std::fs::create_dir_all(state.config.uds_path.parent().unwrap()).unwrap();
    let uds_listener = tokio::net::UnixListener::bind(&state.config.uds_path).unwrap();
    let candidate_authoring_bundle = write_candidate_authoring_bundle(
        fixture_repository_source_root(),
        root.path(),
        &state.identity,
        &runtime_manifest_hash,
        &qualification_runtime_manifest_hash,
        codex.is_some(),
        true,
        scripted_model_origin.as_deref(),
        command_tools_manifest_hash,
    );
    // Independent product qualification always uses enforced descriptor-bound
    // realization mounts. The Codex hosted-controller phase selects its own
    // signed policy only after that evidence has settled.
    let isolation_policy = composed_isolation_policy();
    install_live_engine(
        &mut state,
        candidate_authoring_bundle.root.clone(),
        true,
        isolation_policy.clone(),
    );
    let admitted = admit_external_worker_evidence(&state);
    assert_eq!(admitted.profile, candidate_authoring_bundle.profile);
    let qualification_use = fixture_qualification_use(
        &candidate_authoring_bundle,
        &admitted,
        &runtime_manifest,
        codex.is_some(),
    );
    sign_candidate_qualification_inputs(
        &candidate_authoring_bundle.root,
        &state.identity,
        &runtime_manifest_hash,
        &qualification_runtime_manifest_hash,
        &qualification_use.parameters().unwrap(),
        true,
    );
    install_live_engine(
        &mut state,
        candidate_authoring_bundle.root.clone(),
        true,
        isolation_policy.clone(),
    );
    let after = admit_external_worker_evidence(&state);
    assert_eq!(admitted.source, after.source);
    assert_eq!(admitted.profile, after.profile);
    let trust = ryeos_engine::test_support::live_trust_store();
    state.isolation = ryeos_app::engine_init::load_test_daemon_execution_isolation(
        root.path(),
        &state.config.uds_path,
        &[
            ryeos_engine::test_support::core_bundle_root(),
            ryeos_engine::test_support::standard_bundle_root(),
        ],
        &trust,
        isolation_policy,
    )
    .unwrap();
    let execution_identity = ryeos_app::execution_identity_probe::boot_node_execution_identity(
        &state.state_store,
        &state.identity,
        &state.daemon_build,
    )
    .unwrap();
    let mut extensions = (*state.extensions).clone();
    extensions.insert(execution_identity);
    state.extensions = Arc::new(extensions);
    let runtime_witness_hash = publish_external_runtime_product_witness(&state, &runtime_manifest);
    bind_external_runtimes_to_qualification_verifier(
        &state,
        &[&runtime_manifest_hash, &qualification_runtime_manifest_hash],
    );
    let verifier = dispatch_projectless_public_item(
        &state,
        "tool:fixtures/verify-external-runtime",
        "tool",
        qualification_use.parameters().unwrap(),
    );
    let verifier_thread_id = verifier["thread"]["thread_id"]
        .as_str()
        .expect("public qualification verifier returned no exact thread")
        .to_owned();
    let operator =
        ryeos_app::identity::NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    let handler_context = ryeos_app::handler_context::HandlerContext::new_with_authority(
        operator.principal_id(),
        vec!["*".into()],
        true,
        Some(ryeos_app::identity::AuthorizedKeyPrincipalClass::LocalClient),
        None,
    );
    let qualification_state = Arc::new(state);
    let qualification = ryeos_app::operator_external_content::product_qualification::qualify(
        qualification_state.clone(),
        handler_context,
        ryeos_app::operator_external_content::product_qualification::ProductQualificationRequest {
            witness_hash: runtime_witness_hash.clone(),
            witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
            relationship_name: "auxiliary_to_verifier".into(),
            verifier_chain_root_id: verifier_thread_id.clone(),
            verifier_thread_id,
        },
    )
    .await
    .unwrap();
    let mut state = match Arc::try_unwrap(qualification_state) {
        Ok(state) => state,
        Err(_) => panic!("qualification verifier retained unexpected daemon state ownership"),
    };
    if codex.is_some() {
        // Product qualification and candidate execution are distinct
        // authorities. The verifier above runs with enforced descriptor-bound
        // realization mounts. The actual hosted controller uses the signed
        // trusted/disposable policy that can retain Codex's new connector
        // process group without pretending strict-group containment.
        let hosted_policy = hosted_controller_isolation_policy();
        install_live_engine(
            &mut state,
            candidate_authoring_bundle.root.clone(),
            true,
            hosted_policy.clone(),
        );
        state.isolation = ryeos_app::engine_init::load_test_daemon_execution_isolation(
            root.path(),
            &state.config.uds_path,
            &[
                ryeos_engine::test_support::core_bundle_root(),
                ryeos_engine::test_support::standard_bundle_root(),
            ],
            &trust,
            hosted_policy,
        )
        .unwrap();
        assert!(!state.isolation.is_enforced());
    }
    let runtime_product_selection =
        ryeos_state::external_content::products::composition::ProductSelectionInput {
            target: ryeos_state::external_content::products::composition::ProductSelectionTarget::ExecutionDependency {
                binding: "session_worker".into(),
            },
            selection: ryeos_state::external_content::products::composition::ProductSelection {
                declaration_id: "auxiliary".into(),
                witness_hash: runtime_witness_hash,
                witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                qualification_hash: Some(qualification.qualification_hash),
            },
        };
    let state_authority = state.state_store.pinned_state_authority().unwrap();
    let guard = state_authority.acquire_shared_guard().unwrap();
    let cas = state_authority.cas_store().unwrap();
    let policy = ProjectSnapshotPolicy::new(
        ryeos_state::project_sync::ProjectSyncScope::FullProject,
        vec![],
        vec![],
        Default::default(),
    )
    .unwrap();
    let policy_hash = cas.store_object(&policy.to_value()).unwrap();
    let mut base_files = BTreeMap::new();
    for (path, (bytes, mode)) in signed_candidate_operation_sources(&state.identity) {
        store_project_file(&cas, &mut base_files, &path, &bytes, mode);
    }
    let tree_hash = cas
        .store_object(
            &ProjectTree {
                files: base_files.clone(),
            }
            .to_value(),
        )
        .unwrap();
    let base_snapshot_hash = cas
        .store_object(
            &ProjectSnapshot {
                project_tree_hash: tree_hash.clone(),
                effective_policy_hash: policy_hash.clone(),
                parent_hashes: vec![],
                created_at: "2026-09-21T00:00:00Z".into(),
                message: None,
                source: "external-supervisor-composed-test".into(),
            }
            .to_value(),
        )
        .unwrap();
    for (relative, object_hash) in &base_files {
        ryeos_project_capture::materialize_project_file(
            &state_authority,
            &guard,
            object_hash,
            &project.join(relative),
        )
        .unwrap();
    }
    let base_closure = ryeos_state::project_materialization::VerifiedProjectSnapshotClosure::load(
        &cas,
        &base_snapshot_hash,
    )
    .unwrap();
    let project_materialization = ryeos_state::PinnedProjectMaterialization::verify_from_closure(
        &state_authority,
        &guard,
        &base_closure,
        &project,
    )
    .unwrap();
    drop(guard);
    drop(state_authority);
    if command_tools_manifest_hash.is_none() {
        // Exercise the ordinary signed root-dispatch preflight before the
        // fixture's lower-level placement helper.  The Worker declares one
        // required runtime product slot, so an empty caller selection must be
        // rejected before an external allocation can exist.
        let project_authority = ryeos_state::objects::ExecutionProjectAuthority::pinned(
            "project:external-public-preflight".into(),
            Some(project.clone()),
            base_snapshot_hash.clone(),
            ryeos_state::objects::PinnedProjectRealization::Cow {
                terminal_publication: ryeos_state::objects::PinnedTerminalPublication::RetainResult,
            },
            ryeos_state::objects::EnvironmentAuthority::None,
            Vec::new(),
        )
        .unwrap()
        .with_child_policy(ryeos_state::objects::ChildProjectAuthorityPolicy::Inherit)
        .unwrap();
        let provenance = ryeos_app::execution_provenance::ExecutionProvenance::root_pushed_head(
            project.clone(),
            state.engine.clone(),
            project_lifeline.clone(),
            project_materialization.clone(),
            project_authority,
        )
        .unwrap();
        let scopes = vec!["*".to_owned()];
        let principal = format!("fp:{}", state.identity.fingerprint());
        let plan_ctx = ryeos_engine::contracts::PlanContext {
            requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
                ryeos_engine::contracts::Principal {
                    fingerprint: principal.clone(),
                    scopes: scopes.clone(),
                },
            ),
            project_context: ryeos_engine::contracts::ProjectContext::LocalPath {
                path: project.clone(),
            },
            subject_resolution_authority: provenance.subject_resolution_authority(),
            current_site_id: state.threads.site_id().to_owned(),
            origin_site_id: state.threads.site_id().to_owned(),
            execution_hints: ryeos_engine::contracts::ExecutionHints::default(),
            scheduled_fire: None,
            validate_only: false,
        };
        let context = ryeos_executor::executor::ExecutionContext {
            principal_fingerprint: principal,
            caller_scopes: scopes,
            engine: state.engine.clone(),
            plan_ctx,
            requested_call: None,
        };
        let dispatch_state = state.clone();
        let dispatch_project = project.clone();
        // This composed fixture already has a large TLS/controller stack on
        // the Tokio test worker. Exercise the ordinary dispatch on one
        // explicitly bounded test thread rather than requiring callers to set
        // an ambient RUST_MIN_STACK for the whole suite.
        let error = std::thread::Builder::new()
            .name("external-public-dispatch-refusal".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                let binding = ryeos_app::thread_lifecycle::AdmittedProjectBinding::from_provenance(
                    &context.engine,
                    &context.plan_ctx,
                    &provenance,
                )
                .unwrap();
                let params = serde_json::json!({"credential_profile_id":"credential:fixture"});
                let preflight = ryeos_executor::dispatch::preflight_root_dispatch(
                    "worker_execution:test/external-candidate",
                    "worker_execution",
                    &params,
                    &BTreeMap::new(),
                    &Vec::new(),
                    None,
                    None,
                    &binding,
                    &context,
                    &dispatch_state,
                    None,
                )
                .unwrap();
                let root_admission = preflight.root_admission.unwrap();
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    if let Err(error) = ryeos_executor::dispatch::admit_launch_contract(
                        preflight.root_dispatch_evidence.applicability(),
                        &root_admission,
                        &BTreeMap::new(),
                        &ryeos_state::objects::ExecutionLifecycleAuthority::REQUEST_SCOPED,
                        &provenance,
                        &context,
                        &dispatch_state,
                    )
                    .await
                    {
                        return error;
                    }
                    ryeos_executor::dispatch::dispatch(
                        "worker_execution:test/external-candidate",
                        &ryeos_executor::dispatch::DispatchRequest {
                            launch_mode: "wait",
                            target_site_id: None,
                            validate_only: false,
                            params,
                            ref_bindings: BTreeMap::new(),
                            product_selections: Vec::new(),
                            acting_principal: &context.principal_fingerprint,
                            project_path: &dispatch_project,
                            provenance,
                            lifecycle_authority:
                                ryeos_state::objects::ExecutionLifecycleAuthority::REQUEST_SCOPED,
                            launch_timings: None,
                            original_root_kind: "worker_execution",
                            pre_minted_thread_id: None,
                            usage_subject: None,
                            usage_subject_asserted_by: None,
                            previous_thread_id: None,
                            root_admission: Some(root_admission),
                            root_dispatch_evidence: Some(preflight.root_dispatch_evidence),
                            parent_execution_context: None,
                            effect_authority: None,
                        },
                        &context,
                        &dispatch_state,
                    )
                    .await
                    .unwrap_err()
                })
            })
            .unwrap()
            .join()
            .unwrap();
        let diagnostic = format!("{error:?}");
        assert!(
            diagnostic.contains("product slot has no admitted selection"),
            "ordinary root dispatch failed at the wrong boundary: {error:#}; debug={error:?}"
        );
        assert_eq!(
            ryeos_app::external_placement::fence_external_candidates_after_controller_restart(
                &state,
            )
            .unwrap(),
            0,
            "missing public runtime selection created an external allocation"
        );
    }
    let resolution_materialization =
        ryeos_app::resolution_cache::ResolutionMaterializationBinding::admitted_for_test(
            ryeos_engine::contracts::SubjectResolutionAuthority::PinnedGeneration {
                snapshot_hash: base_snapshot_hash.clone(),
            },
            Some(project.clone()),
            Some(project_lifeline.clone()),
            Some(project_materialization.clone()),
        )
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let controller_port = listener.local_addr().unwrap().port();
    let roots =
        vec![ryeos_external_candidate_supervisor::test_support::TEST_CA_DER_BASE64.to_owned()];
    let controller_contract = ExternalControllerTransportContract {
        schema: 2,
        https_origin: format!("https://localhost:{controller_port}"),
        route_contract: EXTERNAL_CHANNEL_ROUTE_CONTRACT.into(),
        tls_root_bundle_digest: external_tls_root_bundle_digest(&roots).unwrap(),
        connect_timeout_ms: 1_000,
        request_timeout_ms: 2_000,
        maximum_response_bytes: 64 * 1024,
        network_inputs: ExternalNetworkInputPolicy {
            resolver: ExternalNetworkInputSelection {
                source: "/etc/resolv.conf".into(),
                max_bytes: 64 * 1024,
            },
            hosts: ExternalNetworkInputSelection {
                source: "/etc/hosts".into(),
                max_bytes: 64 * 1024,
            },
        },
    };
    let credential = "composed-candidate-secret";
    let settings = serde_json::json!({
        "expected_credential_sha256": lillux::sha256_hex(credential.as_bytes()),
        "maximum_copy_depth": 8,
        "maximum_copy_entries": 1_000,
        "schema": 1,
        "startup_timeout_ms": 5_000,
        "state_root": provider_state.path(),
    });
    if publication == PublicationScenario::PublicDispatch {
        let public_project =
            ryeos_executor::execution::project_source::resolve_pinned_snapshot_context(
                &state,
                &base_snapshot_hash,
                project.clone(),
                "W-external-public-worker",
                ryeos_executor::execution::project_source::PinnedContextRealization::Cow,
            )
            .unwrap();
        let public_engine = public_project.request_engine.clone();
        let public_effective_project = public_project.effective_path.clone();
        let public_lifeline = public_project
            .temp_dir
            .clone()
            .expect("public pinned worker context has no workspace lifeline");
        assert!(public_lifeline.owned_scratch_root().is_ok());
        assert!(
            public_lifeline.borrow_owned_effective_directory().is_err(),
            "fresh creation alone must not authorize transfer before journal/view binding"
        );
        let repeated_creation =
            ryeos_executor::execution::project_source::resolve_pinned_snapshot_context(
                &state,
                &base_snapshot_hash,
                project.clone(),
                "W-external-public-worker",
                ryeos_executor::execution::project_source::PinnedContextRealization::Cow,
            )
            .err()
            .expect("interrupted construction cannot be adopted as a fresh workspace");
        assert!(
            repeated_creation
                .to_string()
                .contains("cannot be freshly created"),
            "{repeated_creation}"
        );
        let public_materialization = public_project
            .pinned_materialization
            .clone()
            .expect("public pinned worker context has no materialization proof");
        let subject = ryeos_engine::contracts::SubjectResolutionAuthority::PinnedGeneration {
            snapshot_hash: base_snapshot_hash.clone(),
        };
        let composition_state = Arc::new(state);
        let program = prepare_external_worker_program_and_binding(
            &composition_state,
            &public_engine,
            &public_effective_project,
            &subject,
            &runtime_product_selection.selection,
            if codex.is_some() {
                real_codex_requirement()
            } else {
                joined_external_candidate_requirement()
            },
            qualification_use.clone(),
        )
        .await;
        let mut state = match Arc::try_unwrap(composition_state) {
            Ok(state) => state,
            Err(_) => {
                panic!("public product composition retained unexpected daemon state ownership")
            }
        };
        ryeos_app::external_placement::test_support::install_test_placement_vault(&mut state);
        ryeos_app::external_placement::test_support::install_precontact_binding_with_settings(
            &mut state,
            &program,
            controller_contract,
            roots,
            settings,
            credential.as_bytes(),
        )
        .unwrap();

        let state = Arc::new(state);
        ryeosd::init_shutdown_channel();
        let uds_server = tokio::spawn(ryeosd::uds::server::serve(uds_listener, state.clone()));
        tokio::task::yield_now().await;
        assert!(
            !uds_server.is_finished(),
            "daemon callback server exited during public worker startup"
        );
        let stop = Arc::new(AtomicBool::new(false));
        let controller_evidence = Arc::new(Mutex::new(Vec::new()));
        let controller = serve_candidate_controller_tls(
            listener,
            state.clone(),
            stop.clone(),
            controller_evidence.clone(),
        );
        if let Some(ready) = scripted_turn_ready {
            ready.send(()).unwrap();
        }
        let admitted_turn_payload = if scripted_turn {
            serde_json::json!({
                "input":[{"type":"text","text":"Inspect the guest workspace and pinned command tools, then write the exact candidate strategy file using remote apply_patch."}]
            })
        } else {
            serde_json::json!({})
        };
        let ref_bindings = command_tools_manifest_hash
            .map(|_| {
                BTreeMap::from([(
                    "environment".to_owned(),
                    "config:test/external-candidate-environment".to_owned(),
                )])
            })
            .unwrap_or_default();
        let (placement, public) = dispatch_pinned_public_worker(
            state.clone(),
            project.clone(),
            public_effective_project,
            public_engine,
            public_lifeline,
            public_materialization,
            base_snapshot_hash.clone(),
            ref_bindings,
            command_tools_manifest_hash.map(str::to_owned),
            vec![runtime_product_selection],
            serde_json::json!({
                "credential_profile_id":"credential:fixture",
                "goal":{
                    "session_start_payload":{},
                    "turn_start_payload":admitted_turn_payload,
                },
                "evidence_attachments":[],
            }),
        );
        stop.store(true, Ordering::Release);
        let handled = controller.join().unwrap();
        let public = match public {
            Ok(public) => public,
            Err(error) => {
                // Preserve this synthetic node's exact CAS/journal on failure,
                // rather than erasing capsule evidence during panic cleanup.
                let retained_node = root.keep();
                // The adapter journal is a separate failure domain. Keep it
                // too: controller intent alone cannot prove occurrence death
                // or recover a lost activation observation. Print paths only,
                // never the private bootstrap/adapter document contents.
                let retained_provider_state = provider_state.keep();
                panic!(
                    "public worker dispatch failed: {error}; placement={placement}; \
                     retained_node={}; retained_provider_state={}; \
                     base_snapshot={base_snapshot_hash}; \
                     thread={:#?}; session={:#?}; commands={:#?}; \
                     controller_evidence={:#?}; channel={:#?}; events={:#?}",
                    retained_node.display(),
                    retained_provider_state.display(),
                    state.state_store.get_thread(&placement).unwrap(),
                    state.state_store.dedicated_session(&placement).unwrap(),
                    state
                        .state_store
                        .dedicated_session_commands(&placement)
                        .unwrap(),
                    controller_evidence.lock().unwrap(),
                    state.state_store.external_execution_channel(&placement),
                    state
                        .state_store
                        .latest_thread_events(&placement, 32)
                        .unwrap(),
                )
            }
        };
        if public["thread"]["status"] != "completed" {
            let retained_node = root.keep();
            let retained_provider_state = provider_state.keep();
            panic!(
                "public worker terminal failed; retained_node={}; retained_provider_state={}; \
                 dispatch={public:#}; commands={:#?}; controller_evidence={:#?}",
                retained_node.display(),
                retained_provider_state.display(),
                state
                    .state_store
                    .dedicated_session_commands(&placement)
                    .unwrap(),
                controller_evidence.lock().unwrap(),
            );
        }
        assert!(
            handled >= 5,
            "public candidate controller handled only {handled} requests; dispatch={public:#}"
        );
        let returned_placement = public["thread"]["thread_id"]
            .as_str()
            .expect("public worker dispatch returned no exact thread")
            .to_owned();
        assert_eq!(returned_placement, placement);
        let session = state
            .state_store
            .dedicated_session(&placement)
            .unwrap()
            .expect("public worker dispatch retained no dedicated session");
        assert_eq!(session.state, "terminal");
        assert_eq!(session.terminal_reason.as_deref(), Some("completed"));
        assert_eq!(
            session.publication_result.as_deref(),
            Some("retained_for_review")
        );
        assert_eq!(
            session
                .bounded_outcome
                .as_ref()
                .map(|outcome| format!("{:?}", outcome.kind))
                .as_deref(),
            Some("Completed")
        );
        let completion = session
            .completion_fence
            .as_ref()
            .expect("public bounded turn retained no completion fence");
        assert_eq!(completion.placement_thread_id, placement);
        let candidate = session
            .candidate_snapshot_hash
            .as_ref()
            .expect("public bounded turn retained no candidate C");
        assert_ne!(candidate, &base_snapshot_hash);
        let workspace = state
            .state_store
            .execution_workspace(&session.workspace_id)
            .unwrap()
            .expect("public bounded turn retained no workspace journal");
        assert_eq!(
            workspace.state,
            ryeos_app::runtime_db::WorkspaceState::Closed
        );
        assert_eq!(workspace.base_snapshot, base_snapshot_hash);
        assert_eq!(
            workspace.frozen_snapshot_hash.as_deref(),
            Some(candidate.as_str())
        );
        assert!(
            state
                .state_store
                .thread_workspace_binding(&placement)
                .unwrap()
                .is_none(),
            "completed public worker retained writable workspace membership"
        );
        let state_authority = state.state_store.pinned_state_authority().unwrap();
        let guard = state_authority.acquire_shared_guard().unwrap();
        let cas = state_authority.cas_store().unwrap();
        let closure = ryeos_state::project_materialization::VerifiedProjectSnapshotClosure::load(
            &cas, candidate,
        )
        .unwrap();
        assert_eq!(
            closure.snapshot().parent_hashes,
            vec![base_snapshot_hash.clone()]
        );
        let candidate_file = closure
            .tree()
            .files()
            .get("candidate-strategy.txt")
            .expect("public bounded turn C omitted the remote candidate edit");
        assert_eq!(
            cas.get_blob(&candidate_file.blob_hash).unwrap().unwrap(),
            b"composed external candidate C\n"
        );
        drop(guard);
        drop(state_authority);

        let commands = state
            .state_store
            .dedicated_session_commands(&placement)
            .unwrap();
        assert_eq!(
            commands.len(),
            2,
            "public bounded turn issued extra commands"
        );
        let turn = commands
            .iter()
            .find(|command| command.payload["route_id"] == "turn.start")
            .expect("public bounded turn retained no exact turn command");
        let contacts_before_replay = controller_evidence.lock().unwrap().len();
        let replay = ryeos_app::dedicated_session_service::execute_command(
            &state,
            &placement,
            &turn.idempotency_key,
            &turn.command_kind,
            turn.payload.clone(),
        )
        .await
        .unwrap();
        assert_eq!(replay["command_sequence"], turn.command_sequence);
        assert_eq!(replay["state"], "completed");
        assert_eq!(replay["result"]["redacted"], true);
        assert_eq!(
            state
                .state_store
                .dedicated_session_commands(&placement)
                .unwrap()
                .len(),
            commands.len(),
            "public replay reserved duplicate provider work"
        );
        assert_eq!(
            controller_evidence.lock().unwrap().len(),
            contacts_before_replay,
            "public replay contacted the external provider"
        );
        if let Some(worker) = scripted_model_worker {
            verify_scripted_routing_results(
                &worker.join().unwrap(),
                &std::fs::read(controller_canary.as_ref().unwrap()).unwrap(),
            )
            .unwrap();
        }
        // Emit only closed, non-secret coordinates after their assertions.
        // Successful fixture state is ephemeral; logs must still distinguish
        // worker evidence from the earlier runtime-qualification root.
        eprintln!(
            "public-worker-evidence: {}",
            serde_json::json!({
                "scope": "in_process_public_dispatch_scripted_provider",
                "chain_root_id": session.chain_root_id,
                "placement_thread_id": placement,
                "session_capsule_hash": session.admitted_capsule_hash,
                "base_snapshot_hash": base_snapshot_hash,
                "candidate_snapshot_hash": candidate,
                "completion_fence": completion,
                "commands": commands.len(),
                "replayed_command_sequence": turn.command_sequence,
                "controller_exchanges_before_replay": contacts_before_replay,
                "controller_exchanges_after_replay": controller_evidence.lock().unwrap().len(),
                "paid_model_contact": false,
                "render_contact": false,
                "simulator_contact": false,
            })
        );
        evaluate_integrate_and_publish_candidate(
            &state,
            &placement,
            &project,
            &base_snapshot_hash,
            publication,
        )
        .await;
        uds_server.abort();
        let _ = uds_server.await;
        return;
    }
    let selections = ryeos_state::external_execution::admission::test_support::qualified_external_candidate_selections(
        &runtime_manifest_hash,
        &if codex.is_some() {
            real_codex_requirement()
        } else {
            joined_external_candidate_requirement()
        },
        &qualification_use,
    )
    .unwrap();
    let admission =
        ryeos_executor::test_support::admit_worker_session_with_selected_products_and_placement(
            &mut state,
            "worker_execution:test/external-candidate",
            &project,
            &base_snapshot_hash,
            command_tools_manifest_hash.map(|hash| {
                ("config:test/external-candidate-environment", hash)
            }),
            command_tools_manifest_hash.map(|_| &resolution_materialization),
            {
                let mut selected = selections.into_inner();
                let runtime = selected.get_mut("auxiliary").unwrap();
                runtime.manifest_kind = ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.into();
                runtime.relationship.required_product.storage =
                    ryeos_state::external_content::products::ProductStorage::LargeContent;
                let bounds = &mut runtime.relationship.required_product.bounds;
                bounds.maximum_file_bytes = 512 * 1024 * 1024;
                bounds.maximum_total_bytes = 1024 * 1024 * 1024;
                ryeos_state::external_content::products::composition::ResolvedExternalProductSelections::new(selected).unwrap()
            },
            if codex.is_some() { real_codex_requirement() } else { joined_external_candidate_requirement() },
            qualification_use.clone(),
            controller_contract,
            roots,
            settings,
            credential,
        )
        .unwrap();
    let capsule_hash = admission.capsule_hash;
    let state_authority = state.state_store.pinned_state_authority().unwrap();
    let guard = state_authority.acquire_shared_guard().unwrap();
    let capsule_value = state_authority
        .cas_store()
        .unwrap()
        .get_object(&capsule_hash)
        .unwrap()
        .unwrap();
    let _capsule =
        ryeos_state::objects::AdmittedPersistentSessionCapsule::from_current_value(&capsule_value)
            .unwrap();
    drop(guard);
    drop(state_authority);

    let (workspace_project, workspace_lifeline) =
        ryeos_app::temp_dir_guard::create_runtime_workspace(
            &root.path().join(".ai/state/cache"),
            "W-external-composed-controller",
        )
        .unwrap();
    if publication == PublicationScenario::StaleBase {
        eprintln!("stale-base fixture: admitted guest, preparing candidate session");
    }
    let placement = "T-00000000-0000-0000-0000-000000000004".to_owned();
    let workspace_id = "W-external-composed-controller";
    let worker_instance_id = "worker-external-composed-controller";
    let admitted_turn_payload = if scripted_turn {
        serde_json::json!({
            "input":[{"type":"text","text":"Inspect the guest workspace and pinned command tools, then write the exact candidate strategy file using remote apply_patch."}]
        })
    } else {
        serde_json::json!({})
    };
    ryeos_app::external_placement::test_support::prepare_ordinary_external_session(
        &mut state,
        &placement,
        workspace_id,
        worker_instance_id,
        &capsule_hash,
        &base_snapshot_hash,
        admission.prepared_runtime_launch,
        admitted_turn_payload.clone(),
        &workspace_project,
        &workspace_lifeline,
    )
    .unwrap();
    // Capture B's exact materialization authority while the workspace still
    // realizes B.  The remote worker is expected to mutate this COW
    // realization into C, so reconstructing B's authority after execution
    // would incorrectly ask the verifier to accept C as B.
    let state_authority = state.state_store.pinned_state_authority().unwrap();
    let guard = state_authority.acquire_shared_guard().unwrap();
    // The disabled test workspace backend returns its retained writable view
    // but deliberately does not spawn the production materializer. Populate
    // that exact retained view from B through the ordinary CAS materializer
    // before admitting its materialization authority.
    for (relative, object_hash) in &base_files {
        ryeos_project_capture::materialize_project_file(
            &state_authority,
            &guard,
            object_hash,
            &workspace_project.join(relative),
        )
        .unwrap();
    }
    let base_closure = ryeos_state::project_materialization::VerifiedProjectSnapshotClosure::load(
        &state_authority.cas_store().unwrap(),
        &base_snapshot_hash,
    )
    .unwrap();
    let base_materialization = ryeos_state::PinnedProjectMaterialization::verify_from_closure(
        &state_authority,
        &guard,
        &base_closure,
        &workspace_project,
    )
    .unwrap();
    drop(guard);
    drop(state_authority);
    let project_authority = ryeos_state::objects::ExecutionProjectAuthority::pinned(
        "project:external-placement-test".into(),
        Some(workspace_project.clone()),
        base_snapshot_hash.clone(),
        ryeos_state::objects::PinnedProjectRealization::Cow {
            terminal_publication: ryeos_state::objects::PinnedTerminalPublication::RetainResult,
        },
        ryeos_state::objects::EnvironmentAuthority::None,
        Vec::new(),
    )
    .unwrap()
    .with_child_policy(ryeos_state::objects::ChildProjectAuthorityPolicy::Inherit)
    .unwrap();
    let provenance = ryeos_app::execution_provenance::ExecutionProvenance::root_pushed_head(
        workspace_project.clone(),
        state.engine.clone(),
        workspace_lifeline.clone(),
        base_materialization,
        project_authority,
    )
    .unwrap();
    let state = Arc::new(state);
    ryeosd::init_shutdown_channel();
    let uds_server = tokio::spawn(ryeosd::uds::server::serve(uds_listener, state.clone()));
    tokio::task::yield_now().await;
    assert!(
        !uds_server.is_finished(),
        "daemon callback server exited during startup"
    );
    let premature_workspace_settlement = state
        .state_store
        .settle_completed_external_workspace_owned(&placement)
        .unwrap_err();
    assert!(
        premature_workspace_settlement
            .to_string()
            .contains("completed freeze boundary")
    );
    assert!(
        state
            .state_store
            .thread_workspace_binding(&placement)
            .unwrap()
            .is_some(),
        "failed premature settlement must retain exact workspace membership"
    );
    let turn_payload = serde_json::json!({
        "route_id":"turn.start",
        "payload":admitted_turn_payload,
    });
    let completion_request_digest = ryeos_state::objects::canonical_value_digest(
        &serde_json::json!({"command_kind":"route","payload":turn_payload.clone()}),
    )
    .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let controller_evidence = Arc::new(Mutex::new(Vec::new()));
    let controller = serve_candidate_controller_tls(
        listener,
        state.clone(),
        stop.clone(),
        controller_evidence.clone(),
    );

    let state_root = ryeos_app::private_artifact_home::home_path(
        &state.config.runtime_state_dir(),
        "external-placement-test",
    )
    .unwrap();
    std::fs::create_dir_all(&state_root).unwrap();
    lillux::PinnedDirectory::open(&state_root)
        .unwrap()
        .unwrap()
        .tighten_owner_private_directory()
        .unwrap();
    let runtime_environment = BTreeMap::from([
        (
            "RYEOS_WORKLOAD_HOME".to_owned(),
            state_root.to_string_lossy().into_owned(),
        ),
        (
            "RYEOS_WORKSPACE".to_owned(),
            workspace_project.to_string_lossy().into_owned(),
        ),
        (
            "RYEOS_STRUCTURED_SESSION_ROUTE_SET".to_owned(),
            "session".to_owned(),
        ),
        (
            "RYEOS_STRUCTURED_SESSION_EFFECT_CLASSES".to_owned(),
            "external_effect,pure_read,session_mutation".to_owned(),
        ),
    ]);
    let identity =
        ryeos_executor::execution::persistent_session::ExclusivePersistentSessionIdentity {
            placement_thread_id: placement.clone(),
            worker_instance_id: worker_instance_id.to_owned(),
            boot_identity_hash: lillux::sha256_hex(b"joined-external-worker-boot"),
            boot_epoch: 1,
            lifecycle_generation: 1,
            control_channel_identity: "fd:joined-external-controller".into(),
            accounting_scope: None,
        };
    let observation_state = state.clone();
    let observation_placement = placement.clone();
    let observation_sink: ryeos_app::persistent_session::PersistentSessionObservationSink =
        Arc::new(move |raw| {
            ryeos_app::dedicated_session_service::ingest_observation_batch(
                &observation_state,
                &observation_placement,
                1,
                raw,
            )
        });
    let start_state = state.clone();
    let start_capsule = capsule_hash.clone();
    let start_workspace = workspace_project.clone();
    let start_lifeline = workspace_lifeline.clone();
    let start_state_root = state_root.clone();
    if scripted_turn || publication == PublicationScenario::StaleBase {
        eprintln!("external fixture: starting exclusive capsule");
    }
    tokio::task::spawn_blocking(move || {
        ryeos_executor::execution::persistent_session::start_exclusive_capsule(
            &start_state,
            &start_capsule,
            &start_workspace,
            start_lifeline,
            Some(&start_state_root),
            &runtime_environment,
            Vec::new(),
            &identity,
            observation_sink,
        )
    })
    .await
    .unwrap()
    .unwrap();
    if scripted_turn || publication == PublicationScenario::StaleBase {
        eprintln!("external fixture: exclusive capsule started");
    }
    if codex.is_some() && !scripted_turn {
        // This profile has no turn or model route. No authentication state is
        // copied into the fresh provider home; environment inspection is the
        // only admitted provider operation.
        assert!(!state_root.join("auth.json").exists());
        let inspected = ryeos_app::dedicated_session_service::execute_command(
            &state, &placement, &format!("handshake:{placement}:inspect"), "route",
            serde_json::json!({"route_id":"environment.inspect","payload":{"environmentId":"ryeos-external-candidate"}}),
        ).await;
        let local = ryeos_app::dedicated_session_service::execute_command(
            &state, &placement, &format!("handshake:{placement}:local-refusal"), "route",
            serde_json::json!({"route_id":"environment.inspect","payload":{"environmentId":"local"}}),
        ).await;
        // Always settle before asserting the inspection, so a refused protocol
        // does not leave a test occurrence running while its owner unwinds.
        let cancelled = ryeos_app::dedicated_session_service::terminate_session(
            &state,
            &placement,
            "cancelled",
            None,
        )
        .await;
        stop.store(true, Ordering::Release);
        let handled = controller.join().unwrap();
        uds_server.abort();
        let _ = uds_server.await;
        cancelled.unwrap();
        let inspected = inspected.unwrap();
        assert_eq!(inspected["state"], "completed", "{inspected}");
        let shell = &inspected["result"]["response"]["result"]["shell"];
        assert!(
            shell["name"].as_str().is_some() && shell["path"].as_str().is_some(),
            "real provider omitted its guest shell identity: {inspected}"
        );
        eprintln!("credential-free guest shell identity: {shell}");
        assert_eq!(
            inspected["result"]["response"]["result"]["cwd"],
            "file:///workspace",
            "real provider did not inspect the remote workspace: {inspected}; evidence={:?}",
            controller_evidence.lock().unwrap()
        );
        let local = local.unwrap();
        assert!(
            local["result"]["response"].get("error").is_some(),
            "local fallback was not refused: {local}"
        );
        assert!(handled >= 5);
        let session = state
            .state_store
            .dedicated_session(&placement)
            .unwrap()
            .unwrap();
        assert_eq!(session.state, "terminal");
        assert_eq!(session.terminal_reason.as_deref(), Some("cancelled"));
        assert!(session.completion_fence.is_none());
        assert!(session.candidate_snapshot_hash.is_none());
        return;
    }
    assert!(
        state
            .state_store
            .dedicated_session_commands(&placement)
            .unwrap()
            .is_empty(),
        "external session start inherited a command before the first route call"
    );
    if scripted_turn || publication == PublicationScenario::StaleBase {
        eprintln!("external fixture: dispatching session.start");
    }
    let session_start = ryeos_app::dedicated_session_service::execute_command(
        &state,
        &placement,
        &format!("bounded:{placement}:session-start:attempt:1"),
        "route",
        serde_json::json!({"route_id":"session.start","payload":{}}),
    )
    .await
    .unwrap_or_else(|error| {
        let commands = state
            .state_store
            .dedicated_session_commands(&placement)
            .unwrap();
        panic!(
            "joined session start failed: {error:#}; retained commands: {commands:#?}; controller evidence: {:?}",
            controller_evidence.lock().unwrap()
        )
    });
    if scripted_turn || publication == PublicationScenario::StaleBase {
        eprintln!("external fixture: session.start returned");
    }
    assert_eq!(session_start["state"], "completed");
    assert!(
        session_start["result"]["response"].get("error").is_none(),
        "session start was refused: {session_start}; controller evidence: {:?}",
        controller_evidence.lock().unwrap()
    );
    if let Some(ready) = scripted_turn_ready {
        ready.send(()).unwrap();
    }
    let turn_idempotency_key = format!("bounded:{placement}:turn-start:attempt:1");
    let turn = ryeos_app::dedicated_session_service::execute_command(
        &state,
        &placement,
        &turn_idempotency_key,
        "route",
        turn_payload.clone(),
    )
    .await
    .unwrap();
    assert_eq!(turn["state"], "completed");
    let turn_response_digest =
        ryeos_state::objects::canonical_value_digest(&turn["result"]).unwrap();
    let command_sequence = turn["command_sequence"].as_u64().unwrap();
    let deadline = Instant::now() + Duration::from_secs(40);
    let observation = loop {
        let observation = ryeos_app::dedicated_session_service::command_observation(
            &state,
            &placement,
            command_sequence,
        )
        .unwrap();
        if observation["operation"]["state"] == "completed" {
            break observation;
        }
        assert!(
            Instant::now() < deadline,
            "real Codex turn did not complete: {observation}; controller={:?}; rollout_kinds={:?}",
            controller_evidence.lock().unwrap(),
            private_fixture_rollout_kinds(&state_root),
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    if publication == PublicationScenario::StaleBase {
        eprintln!("stale-base fixture: command completed, terminating session");
    }
    assert_eq!(observation["operation"]["state"], "completed");
    let guest_shell_succeeded = if let Some(worker) = scripted_model_worker {
        let requests = worker.join().unwrap();
        verify_scripted_routing_results(
            &requests,
            &std::fs::read(controller_canary.as_ref().unwrap()).unwrap(),
        )
        .unwrap();
        assert!(
            !workspace_project.join("candidate-strategy.txt").exists(),
            "remote patch wrote the trusted controller workspace"
        );
        Some(true)
    } else {
        None
    };
    let guest_edit_before_capture = if scripted_turn {
        let occurrence_id =
            ryeos_app::external_placement::test_support::external_occurrence_id(&state, &placement)
                .unwrap();
        let private = provider_state
            .path()
            .join(occurrence_id)
            .join("candidate-private");
        let roots = std::fs::read_dir(private)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            roots.len(),
            1,
            "native fixture did not retain exactly one private candidate"
        );
        Some(roots[0].path().join("candidate-strategy.txt").exists())
    } else {
        None
    };
    if scripted_turn {
        let commands_before_replay = state
            .state_store
            .dedicated_session_commands(&placement)
            .unwrap()
            .len();
        let replay = ryeos_app::dedicated_session_service::execute_command(
            &state,
            &placement,
            &turn_idempotency_key,
            "route",
            turn_payload.clone(),
        )
        .await
        .unwrap();
        assert_eq!(replay["command_sequence"], turn["command_sequence"]);
        assert_eq!(replay["state"], "completed");
        assert_eq!(replay["result"]["redacted"], true);
        assert_eq!(replay["result"]["response_digest"], turn_response_digest);
        assert_eq!(
            state
                .state_store
                .dedicated_session_commands(&placement)
                .unwrap()
                .len(),
            commands_before_replay,
            "settled turn retry reserved another provider command"
        );
    }
    let completion_fence: ryeos_app::dedicated_session_service::HostedCommandCompletionFence =
        serde_json::from_value(observation["completion_fence"].clone()).unwrap();
    assert_eq!(completion_fence.request_digest, completion_request_digest);
    ryeos_app::dedicated_session_service::terminate_session(
        &state,
        &placement,
        "completed",
        Some(&completion_fence),
    )
    .await
    .unwrap();
    if publication == PublicationScenario::StaleBase {
        eprintln!("stale-base fixture: termination returned, awaiting import");
    }
    let occurrence_id =
        ryeos_app::external_placement::test_support::external_occurrence_id(&state, &placement)
            .unwrap();
    let terminal = provider_state
        .path()
        .join(&occurrence_id)
        .join("terminal.json");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !terminal.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if !terminal.exists() {
        let terminal_evidence = terminal
            .exists()
            .then(|| std::fs::read_to_string(&terminal).unwrap());
        stop.store(true, Ordering::Release);
        let handled = controller.join().unwrap();
        panic!(
            "external candidate did not settle/import; terminal={terminal_evidence:?}; handled={handled}; import_failure={:?}; controller={:?}",
            state.external_candidate_imports.last_failure_for_test(),
            controller_evidence.lock().unwrap().as_slice()
        );
    }
    stop.store(true, Ordering::Release);
    let handled = controller.join().unwrap();
    assert!(
        handled >= 5,
        "candidate controller handled only {handled} requests"
    );
    if scripted_turn {
        let replay = ryeos_app::dedicated_session_service::execute_command(
            &state,
            &placement,
            &turn_idempotency_key,
            "route",
            turn_payload,
        )
        .await
        .unwrap();
        assert_eq!(replay["command_sequence"], turn["command_sequence"]);
        assert_eq!(replay["state"], "completed");
        assert_eq!(replay["result"]["redacted"], true);
        assert_eq!(replay["result"]["response_digest"], turn_response_digest);
        assert_eq!(
            state
                .state_store
                .dedicated_session_commands(&placement)
                .unwrap()
                .len(),
            2,
            "terminal turn retry reserved another provider command"
        );
    }
    let imported =
        ryeos_app::external_placement::completed_external_candidate_generation(&state, &placement)
            .unwrap()
            .expect("controller did not retain imported candidate C");
    if publication == PublicationScenario::StaleBase {
        eprintln!("stale-base fixture: imported C, freezing candidate");
    }
    assert_ne!(imported.snapshot_hash, base_snapshot_hash);
    assert!(imported.output_capture_hash.is_none());
    assert_eq!(
        ryeos_app::external_placement::completed_external_candidate_generation(&state, &placement,)
            .unwrap(),
        Some(imported.clone()),
        "the ordinary terminal-result owner must select imported C"
    );
    assert_eq!(
        ryeos_app::external_placement::advance_external_candidate_completion(
            &state,
            &placement,
            &completion_request_digest,
        )
        .unwrap(),
        ryeos_app::external_placement::ExternalCandidateCompletionProgress::Imported(
            imported.clone()
        )
    );

    let launch_owner = state
        .state_store
        .get_launch_claim(&placement)
        .unwrap()
        .unwrap()
        .claimed_by;
    assert_eq!(
        ryeos_executor::test_support::freeze_completed_external_candidate(
            &state,
            &provenance,
            &placement,
            &launch_owner,
        )
        .unwrap(),
        imported,
        "the ordinary workspace freeze owner must bind imported C"
    );
    let frozen_workspace = state
        .state_store
        .execution_workspace("W-external-composed-controller")
        .unwrap()
        .unwrap();
    assert_eq!(
        frozen_workspace.state,
        ryeos_app::runtime_db::WorkspaceState::Freezing
    );
    assert_eq!(
        frozen_workspace.frozen_snapshot_hash.as_deref(),
        Some(imported.snapshot_hash.as_str())
    );
    ryeos_executor::test_support::finalize_completed_external_candidate(
        &state,
        &provenance,
        &placement,
        &imported.snapshot_hash,
    )
    .unwrap();
    assert_eq!(
        state
            .state_store
            .execution_workspace("W-external-composed-controller")
            .unwrap()
            .unwrap()
            .state,
        ryeos_app::runtime_db::WorkspaceState::Closed
    );
    let frozen_session = state
        .state_store
        .dedicated_session(&placement)
        .unwrap()
        .unwrap();
    assert_eq!(frozen_session.state, "terminal");
    assert_eq!(
        frozen_session.publication_result.as_deref(),
        Some("retained_for_review")
    );
    assert_eq!(frozen_session.terminal_reason.as_deref(), Some("completed"));
    assert_eq!(
        frozen_session.completion_fence.as_ref(),
        Some(&completion_fence)
    );
    assert_eq!(
        frozen_session.candidate_snapshot_hash.as_deref(),
        Some(imported.snapshot_hash.as_str())
    );

    let state_authority = state.state_store.pinned_state_authority().unwrap();
    let guard = state_authority.acquire_shared_guard().unwrap();
    let cas = state_authority.cas_store().unwrap();
    let closure = ryeos_state::project_materialization::VerifiedProjectSnapshotClosure::load(
        &cas,
        &imported.snapshot_hash,
    )
    .unwrap();
    assert_eq!(
        closure.snapshot().parent_hashes,
        vec![base_snapshot_hash.clone()]
    );
    let candidate_file = closure
        .tree()
        .files()
        .get("candidate-strategy.txt")
        .unwrap_or_else(|| {
            let rooted_blobs = state
                .state_store
                .external_execution_blob_roots()
                .unwrap()
                .into_iter()
                .filter_map(|hash| cas.get_blob(&hash).unwrap())
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .collect::<Vec<_>>();
            panic!(
                "candidate C omitted the remote edit; guest_file_before_capture={guest_edit_before_capture:?}; controller_file={:?}; files={:?}; rooted_blobs={rooted_blobs:?}",
                workspace_project.join("candidate-strategy.txt").exists(),
                closure.tree().files().keys().collect::<Vec<_>>(),
            )
        });
    if scripted_turn {
        assert_eq!(guest_edit_before_capture, Some(true));
    }
    assert_eq!(
        cas.get_blob(&candidate_file.blob_hash).unwrap().unwrap(),
        b"composed external candidate C\n"
    );
    drop(guard);
    drop(state_authority);

    evaluate_integrate_and_publish_candidate(
        &state,
        &placement,
        &workspace_project,
        &base_snapshot_hash,
        publication,
    )
    .await;
    uds_server.abort();
    let _ = uds_server.await;
    assert_ne!(
        guest_shell_succeeded,
        Some(false),
        "guest shell and admitted rg must run before qualifying Codex authoring"
    );
}

// Continue from the public session owner's retained C. Both public dispatch
// and the lower-level transport fixture must use the same independent
// evaluation/integration/publication path.
async fn evaluate_integrate_and_publish_candidate(
    state: &Arc<ryeos_app::state::AppState>,
    placement: &str,
    workspace_project: &Path,
    base_snapshot_hash: &str,
    publication: PublicationScenario,
) {
    use ryeos_state::objects::ProjectSnapshot;

    let frozen_session = state
        .state_store
        .dedicated_session(placement)
        .unwrap()
        .unwrap();
    let (source, _, _) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(
            &frozen_session.chain_root_id,
            &frozen_session.placement_thread_id,
        )
        .unwrap()
        .expect("completed candidate retains its source authority");
    let source_display_path = match &source.project_authority {
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            display_path: Some(path),
            base_snapshot_hash: source_base,
            ..
        } if source_base == base_snapshot_hash => path,
        _ => panic!("candidate must retain the exact pinned base authority"),
    };
    assert_eq!(Path::new(source_display_path), workspace_project);
    let candidate_snapshot_hash = frozen_session
        .candidate_snapshot_hash
        .clone()
        .expect("completed public session must retain exact C");
    let authority = state.state_store.pinned_state_authority().unwrap();
    let guard = authority.acquire_shared_guard().unwrap();
    let cas = authority.cas_store().unwrap();
    let closure = ryeos_state::project_materialization::VerifiedProjectSnapshotClosure::load(
        &cas,
        &candidate_snapshot_hash,
    )
    .unwrap();
    let base = ryeos_state::project_materialization::VerifiedProjectSnapshotClosure::load(
        &cas,
        base_snapshot_hash,
    )
    .unwrap();
    let tree_hash = base.snapshot().project_tree_hash.clone();
    let policy_hash = base.snapshot().effective_policy_hash.clone();
    drop(guard);
    drop(authority);
    let candidate_validation_hash = frozen_session
        .candidate_validation_hash
        .clone()
        .expect("frozen candidate has canonical validation identity");
    let workflow_chain_root = frozen_session.chain_root_id.clone();
    let service_owner = frozen_session.owner_principal.clone();
    let validated = execute_recorded_service(
        &state,
        &service_owner,
        "service:worker-executions/validate-candidate-closure-and-base",
        serde_json::json!({
            "chain_root_id":workflow_chain_root,
            "candidate_snapshot_hash":candidate_snapshot_hash,
            "candidate_validation_hash":candidate_validation_hash,
        }),
    )
    .await;
    assert_eq!(
        validated.value["qualification"], "closure_verified",
        "candidate C must pass the ordinary closure/base validator"
    );

    let principal_key = ryeos_state::refs::principal_storage_key(&service_owner).unwrap();
    let project_hash = ryeos_state::refs::deployed_project_key(
        source_display_path
            .to_str()
            .expect("source display path is UTF-8"),
    );
    let state_authority = state.state_store.pinned_state_authority().unwrap();
    let guard = state_authority.acquire_shared_guard().unwrap();
    state
        .state_store
        .write_project_head_ref(
            &principal_key,
            &project_hash,
            &base_snapshot_hash,
            &ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity),
            &guard,
        )
        .unwrap();
    drop(guard);
    drop(state_authority);

    let evaluator_parameters = serde_json::json!({
        "base_snapshot_hash":base_snapshot_hash,
        "candidate_snapshot_hash":candidate_snapshot_hash,
        "expect_integration":false,
    });
    let evaluator_launch_id = format!(
        "L-{}",
        &lillux::sha256_hex(b"external-candidate-e2e-evaluate-c")[..32]
    );
    let evaluator_launch = execute_recorded_service(
        &state,
        &service_owner,
        "service:worker-executions/start-candidate-evaluation",
        serde_json::json!({
            "chain_root_id":workflow_chain_root,
            "candidate_snapshot_hash":candidate_snapshot_hash,
            "candidate_validation_hash":candidate_validation_hash,
            "evaluator_item_ref":"tool:test/external-candidate/evaluate",
            "evaluator_parameters":evaluator_parameters,
            "evaluator_ref_bindings":{},
            "launch_id":evaluator_launch_id,
        }),
    )
    .await;
    let evaluator_thread_id = evaluator_launch.value["thread_id"]
        .as_str()
        .expect("evaluator launch returned its exact thread")
        .to_owned();
    assert_eq!(
        evaluator_launch.value["chain_root_id"], evaluator_thread_id,
        "candidate evaluator must be an independent root chain"
    );
    let evaluator_terminal = wait_for_terminal_thread(&state, &evaluator_thread_id).await;
    if publication == PublicationScenario::StaleBase {
        eprintln!("stale-base fixture: independent C evaluation settled");
    }
    assert_eq!(
        evaluator_terminal
            .result
            .as_ref()
            .and_then(|value| value["accepted"].as_bool()),
        Some(true),
        "independent evaluator must accept exact candidate C"
    );

    let evaluator_capsule_hash = evaluator_terminal
        .admitted_launch_capsule_hash
        .clone()
        .expect("candidate evaluator has an admitted capsule");
    let evaluator_capsule = state
        .state_store
        .admitted_launch_capsule(&evaluator_thread_id)
        .unwrap()
        .expect("candidate evaluator capsule remains retained");
    let evaluator_request =
        ryeos_app::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
            &evaluator_capsule,
        )
        .unwrap();
    let integration_launch_id = format!(
        "L-{}",
        &lillux::sha256_hex(b"external-candidate-e2e-integrate-d")[..32]
    );
    let qualified_c = execute_recorded_service(
        &state,
        &service_owner,
        "service:worker-executions/qualify-candidate",
        serde_json::json!({
            "chain_root_id":workflow_chain_root,
            "candidate_snapshot_hash":candidate_snapshot_hash,
            "candidate_validation_hash":candidate_validation_hash,
            "evaluator_chain_root_id":evaluator_thread_id,
            "evaluator_terminal_thread_id":evaluator_thread_id,
            "evaluator_capsule_hash":evaluator_capsule_hash,
            "evaluator_item_ref":"tool:test/external-candidate/evaluate",
            "evaluator_definition_digest":evaluator_request.effective_definition_digest(),
            "evaluator_parameters_digest":evaluator_request.admitted_parameters_digest().unwrap(),
            "integration_launch_id":integration_launch_id,
        }),
    )
    .await;
    assert_eq!(qualified_c.value["accepted"], true);
    let accepted_evaluation_hash = qualified_c.value["candidate_evaluation_hash"]
        .as_str()
        .expect("candidate C qualification returned exact testimony")
        .to_owned();

    let integration_launch = execute_recorded_service(
        &state,
        &service_owner,
        "service:worker-executions/start-candidate-integration",
        serde_json::json!({
            "chain_root_id":workflow_chain_root,
            "candidate_snapshot_hash":candidate_snapshot_hash,
            "candidate_validation_hash":candidate_validation_hash,
            "accepted_evaluation_hash":accepted_evaluation_hash,
            "authoring_wrapper_item_ref":"tool:external-candidate-authoring/integrate",
            "authoring_parameters":{
                "item_ref":"knowledge:test/external-candidate/integration",
                "content":"---\ncategory: test/external-candidate\ntags: [integration, e2e]\nversion: \"1.0.0\"\ndescription: Exact integrated candidate D testimony.\n---\n\n# Integration D\n\nAuthored through the capsule-scoped integration boundary.\n",
                "mode":"create",
                "format_ext":".md"
            },
            "authoring_ref_bindings":{},
            "launch_id":integration_launch_id,
        }),
    )
    .await;
    let integration_thread_id = integration_launch.value["thread_id"]
        .as_str()
        .expect("integration launch returned its exact thread")
        .to_owned();
    let integration_terminal = wait_for_terminal_thread(&state, &integration_thread_id).await;
    if publication == PublicationScenario::StaleBase {
        eprintln!("stale-base fixture: integration D settled");
    }
    let integration_capsule_hash = integration_terminal
        .admitted_launch_capsule_hash
        .clone()
        .expect("integration D has an admitted capsule");
    let integration_result = integration_terminal
        .result
        .as_ref()
        .expect("integration D has canonical terminal testimony");
    let integrated_snapshot_hash = integration_result["result_candidate_snapshot_hash"]
        .as_str()
        .expect("integration D result names its exact generation")
        .to_owned();
    let integrated_validation_hash = integration_result["result_candidate_validation_hash"]
        .as_str()
        .expect("integration D result names its validation identity")
        .to_owned();
    assert_ne!(integrated_snapshot_hash, candidate_snapshot_hash);
    let state_authority = state.state_store.pinned_state_authority().unwrap();
    let guard = state_authority.acquire_shared_guard().unwrap();
    let cas = state_authority.cas_store().unwrap();
    let integrated_closure =
        ryeos_state::project_materialization::VerifiedProjectSnapshotClosure::load(
            &cas,
            &integrated_snapshot_hash,
        )
        .unwrap();
    assert_eq!(
        integrated_closure.snapshot().parent_hashes,
        vec![candidate_snapshot_hash.clone()],
        "integration D must be a direct descendant of the exact accepted C"
    );
    assert_eq!(
        integrated_closure.snapshot().effective_policy_hash,
        closure.snapshot().effective_policy_hash,
        "integration D must retain C's exact snapshot policy"
    );
    assert!(
        integrated_closure
            .tree()
            .files()
            .contains_key(".ai/knowledge/test/external-candidate/integration.md"),
        "integration D must contain the daemon-authored accepted item"
    );
    drop(guard);
    drop(state_authority);

    let integrated_evaluator_parameters = serde_json::json!({
        "base_snapshot_hash":base_snapshot_hash,
        "candidate_snapshot_hash":integrated_snapshot_hash,
        "expect_integration":true,
    });
    let integrated_evaluator_launch_id = format!(
        "L-{}",
        &lillux::sha256_hex(b"external-candidate-e2e-evaluate-d")[..32]
    );
    let integrated_evaluator_launch = execute_recorded_service(
        &state,
        &service_owner,
        "service:worker-executions/start-candidate-evaluation",
        serde_json::json!({
            "chain_root_id":workflow_chain_root,
            "candidate_snapshot_hash":integrated_snapshot_hash,
            "candidate_validation_hash":integrated_validation_hash,
            "evaluator_item_ref":"tool:test/external-candidate/evaluate",
            "evaluator_parameters":integrated_evaluator_parameters,
            "evaluator_ref_bindings":{},
            "integration":{
                "chain_root_id":integration_thread_id,
                "terminal_thread_id":integration_thread_id,
                "capsule_hash":integration_capsule_hash,
            },
            "launch_id":integrated_evaluator_launch_id,
        }),
    )
    .await;
    let integrated_evaluator_thread_id = integrated_evaluator_launch.value["thread_id"]
        .as_str()
        .expect("integrated evaluator launch returned its exact thread")
        .to_owned();
    let integrated_evaluator_terminal =
        wait_for_terminal_thread(&state, &integrated_evaluator_thread_id).await;
    if publication == PublicationScenario::StaleBase {
        eprintln!("stale-base fixture: independent D evaluation settled");
    }
    assert_eq!(
        integrated_evaluator_terminal
            .result
            .as_ref()
            .and_then(|value| value["accepted"].as_bool()),
        Some(true),
        "independent evaluator must accept exact integration D"
    );

    let integrated_evaluator_capsule_hash = integrated_evaluator_terminal
        .admitted_launch_capsule_hash
        .clone()
        .expect("integrated evaluator has an admitted capsule");
    let integrated_evaluator_capsule = state
        .state_store
        .admitted_launch_capsule(&integrated_evaluator_thread_id)
        .unwrap()
        .expect("integrated evaluator capsule remains retained");
    let integrated_evaluator_request =
        ryeos_app::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
            &integrated_evaluator_capsule,
        )
        .unwrap();
    let qualified_d = execute_recorded_service(
        &state,
        &service_owner,
        "service:worker-executions/qualify-candidate",
        serde_json::json!({
            "chain_root_id":workflow_chain_root,
            "candidate_snapshot_hash":integrated_snapshot_hash,
            "candidate_validation_hash":integrated_validation_hash,
            "evaluator_chain_root_id":integrated_evaluator_thread_id,
            "evaluator_terminal_thread_id":integrated_evaluator_thread_id,
            "evaluator_capsule_hash":integrated_evaluator_capsule_hash,
            "evaluator_item_ref":"tool:test/external-candidate/evaluate",
            "evaluator_definition_digest":integrated_evaluator_request.effective_definition_digest(),
            "evaluator_parameters_digest":integrated_evaluator_request.admitted_parameters_digest().unwrap(),
            "integration":{
                "chain_root_id":integration_thread_id,
                "terminal_thread_id":integration_thread_id,
                "capsule_hash":integration_capsule_hash,
            },
        }),
    )
    .await;
    assert_eq!(qualified_d.value["accepted"], true);

    if publication == PublicationScenario::StaleBase {
        let authority = state.state_store.pinned_state_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let competing_snapshot_hash = authority
            .cas_store()
            .unwrap()
            .store_object(
                &ProjectSnapshot {
                    project_tree_hash: tree_hash.clone(),
                    effective_policy_hash: policy_hash.clone(),
                    parent_hashes: vec![base_snapshot_hash.to_owned()],
                    created_at: "2026-09-22T00:00:01Z".into(),
                    message: None,
                    source: "external-candidate-competing-head-test".into(),
                }
                .to_value(),
            )
            .unwrap();
        state
            .state_store
            .advance_project_head_ref(
                &principal_key,
                &project_hash,
                &competing_snapshot_hash,
                &base_snapshot_hash,
                &ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity),
                &guard,
            )
            .unwrap();
        drop(guard);
        drop(authority);

        let publication_attempt = try_execute_recorded_service(
            &state,
            &service_owner,
            "service:worker-executions/publish",
            serde_json::json!({
                "chain_root_id":workflow_chain_root,
                "expected_previous_hash":base_snapshot_hash,
            }),
        )
        .await;
        let error = match publication_attempt {
            Ok(result) => panic!(
                "stale B→D publication unexpectedly succeeded: {}",
                result.value
            ),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("publication conflict"),
            "stale B→D publication failed at the wrong boundary: {error:#}"
        );
        assert_eq!(
            state
                .state_store
                .with_state_db(|db| db.read_project_head(&principal_key, &project_hash))
                .unwrap()
                .as_deref(),
            Some(competing_snapshot_hash.as_str()),
            "stale B→D publication replaced the competing HEAD"
        );
        return;
    }

    let published = execute_recorded_service(
        &state,
        &service_owner,
        "service:worker-executions/publish",
        serde_json::json!({
            "chain_root_id":workflow_chain_root,
            "expected_previous_hash":base_snapshot_hash,
        }),
    )
    .await;
    assert_eq!(published.value["published"], true);
    assert_eq!(published.value["snapshot_hash"], integrated_snapshot_hash);
    assert_eq!(published.value["previous_hash"], base_snapshot_hash);
    let publication_replay = execute_recorded_service(
        &state,
        &service_owner,
        "service:worker-executions/publish",
        serde_json::json!({
            "chain_root_id":workflow_chain_root,
            "expected_previous_hash":base_snapshot_hash,
        }),
    )
    .await;
    assert_eq!(publication_replay.value["published"], true);
    assert_eq!(publication_replay.value["idempotent"], true);
    assert_eq!(
        publication_replay.value["snapshot_hash"],
        integrated_snapshot_hash
    );

    assert_eq!(
        state
            .state_store
            .with_state_db(|db| db.read_project_head(&principal_key, &project_hash))
            .unwrap()
            .as_deref(),
        Some(integrated_snapshot_hash.as_str()),
        "authenticated compare-and-swap publication must return exact D"
    );
    eprintln!(
        "candidate-disposition-evidence: {}",
        serde_json::json!({
            "scope": "fixture_evaluation_integration_and_publication",
            "chain_root_id": workflow_chain_root,
            "placement_thread_id": placement,
            "base_snapshot_hash": base_snapshot_hash,
            "candidate_snapshot_hash": candidate_snapshot_hash,
            "candidate_validation_hash": candidate_validation_hash,
            "candidate_evaluation_hash": accepted_evaluation_hash,
            "candidate_evaluator_thread_id": evaluator_thread_id,
            "candidate_evaluator_capsule_hash": evaluator_capsule_hash,
            "candidate_evaluator_definition": evaluator_request.effective_definition_digest(),
            "integration_thread_id": integration_thread_id,
            "integration_capsule_hash": integration_capsule_hash,
            "integrated_snapshot_hash": integrated_snapshot_hash,
            "integrated_validation_hash": integrated_validation_hash,
            "integrated_evaluator_thread_id": integrated_evaluator_thread_id,
            "integrated_evaluator_capsule_hash": integrated_evaluator_capsule_hash,
            "integrated_evaluator_definition": integrated_evaluator_request.effective_definition_digest(),
            "publication_invocation_id": published.invocation_id,
            "publication_replay_invocation_id": publication_replay.invocation_id,
            "publication_replay_idempotent": publication_replay.value["idempotent"],
        })
    );
}

#[test]
fn lost_allocation_response_reconciles_the_original_occurrence() {
    let adapter = executable(env!(
        "CARGO_BIN_EXE_ryeos-synthetic-external-lifecycle-adapter"
    ));
    let supervisor = executable(env!(
        "CARGO_BIN_EXE_ryeos-synthetic-external-candidate-supervisor"
    ));
    let launcher = executable(env!(
        "CARGO_BIN_EXE_ryeos-synthetic-external-candidate-launcher"
    ));
    let state = tempfile::tempdir().unwrap();
    lillux::PinnedDirectory::open(state.path())
        .unwrap()
        .unwrap()
        .tighten_owner_private_directory()
        .unwrap();
    let credential = b"synthetic-secret";
    let settings = lillux::canonical_json(&serde_json::json!({
        "expected_credential_sha256": lillux::sha256_hex(credential),
        "faults": ["lose_first_allocation_response"],
        "maximum_copy_depth": 8,
        "maximum_copy_entries": 1000,
        "schema": 1,
        "startup_timeout_ms": 1000,
        "state_root": state.path(),
    }))
    .unwrap()
    .into_bytes();
    let common = LifecycleOperationCommon {
        schema: 1,
        protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
        operation_id: "f".repeat(64),
        binding_hash: "b".repeat(64),
        settings_digest: lillux::sha256_hex(&settings),
    };
    let reservation = AllocationReservation {
        placement_thread_id: "T-synthetic-lost-allocation-response".into(),
        admitted_capsule_hash: "c".repeat(64),
        base_snapshot_hash: "d".repeat(64),
        request_digest: "e".repeat(64),
        maximum_lifetime_seconds: 60,
        contact_deadline_ms: 1,
    };
    let allocate = LifecycleAdapterRequest::Allocate {
        common: common.clone(),
        reservation: reservation.clone(),
    };
    let error = try_invoke_operation(
        &adapter,
        &supervisor,
        &launcher,
        &settings,
        credential,
        &allocate,
        None,
        Vec::new(),
        lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(5)),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("external lifecycle adapter failed: unsuccessful_exit"),
        "{error:#}"
    );
    let marker: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            state
                .path()
                .join(format!("occ-{}", &reservation.request_digest[..48]))
                .join("fault-allocation-response-lost.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(marker["fault"], "lose_first_allocation_response");
    assert_eq!(marker["request_digest"], reservation.request_digest);

    let reconcile = LifecycleAdapterRequest::ReconcileAllocation {
        common,
        reservation: reservation.clone(),
    };
    let reconciled: LifecycleAdapterResponse = from_json_slice_strict(
        &invoke_operation(
            &adapter,
            &supervisor,
            &launcher,
            &settings,
            credential,
            &reconcile,
            None,
            Vec::new(),
        ),
        MAX_LIFECYCLE_RESPONSE_BYTES,
    )
    .unwrap();
    reconciled.validate_for(&reconcile).unwrap();
    assert!(matches!(
        reconciled,
        LifecycleAdapterResponse::AllocationBound { occurrence_id, .. }
            if occurrence_id == format!("occ-{}", &reservation.request_digest[..48])
    ));
}

#[test]
fn activation_launches_the_real_supervisor_boundary_and_terminates_exactly() {
    exercise_activation_fault("lose_first_activation_response");
}

#[test]
fn activation_staging_fault_is_fenced_by_fresh_adapter_process() {
    exercise_activation_fault("fail_after_staging_intent");
}

#[test]
fn activation_spawn_intent_fault_remains_uncertain_across_adapter_processes() {
    exercise_activation_fault("fail_after_spawn_intent");
}

// Harness-owned directories are removed after success, but preserved on panic
// so the exact provider journal and input closure survive a failed assertion.
struct ActivationFixtureDirectory {
    label: &'static str,
    directory: Option<tempfile::TempDir>,
}

impl ActivationFixtureDirectory {
    fn new(label: &'static str) -> Self {
        Self {
            label,
            directory: Some(tempfile::tempdir().unwrap()),
        }
    }

    fn path(&self) -> &Path {
        self.directory.as_ref().unwrap().path()
    }
}

impl Drop for ActivationFixtureDirectory {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let retained = self.directory.take().unwrap().keep();
            // Harness evidence only. Never print bootstrap or credential bytes.
            eprintln!(
                "activation fixture retained {}={}",
                self.label,
                retained.display()
            );
        }
    }
}

// The same real executable protocol is used for all cuts. A fresh adapter
// process reconciles the retained occurrence; this is not daemon-restart or
// installed-provider qualification.
fn exercise_activation_fault(fault: &str) {
    use ryeos_state::objects::{ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree};

    let adapter = executable(env!(
        "CARGO_BIN_EXE_ryeos-synthetic-external-lifecycle-adapter"
    ));
    let supervisor = executable(env!(
        "CARGO_BIN_EXE_ryeos-synthetic-external-candidate-supervisor"
    ));
    let launcher = executable(env!(
        "CARGO_BIN_EXE_ryeos-synthetic-external-candidate-launcher"
    ));
    let (_, _, _) = artifact(&adapter);
    let (supervisor_hash, _, _) = artifact(&supervisor);
    let (launcher_hash, _, _) = artifact(&launcher);

    let state = ActivationFixtureDirectory::new("provider_state");
    let base_store = ActivationFixtureDirectory::new("base_store");
    let runtime = ActivationFixtureDirectory::new("runtime_input");
    for path in [state.path(), base_store.path(), runtime.path()] {
        lillux::PinnedDirectory::open(path)
            .unwrap()
            .unwrap()
            .tighten_owner_private_directory()
            .unwrap();
    }
    std::fs::create_dir(runtime.path().join("bin")).unwrap();
    std::fs::write(
        runtime.path().join("bin/codex"),
        b"not reached by this lifecycle test",
    )
    .unwrap();
    std::fs::set_permissions(
        runtime.path().join("bin/codex"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let runtime = lillux::PinnedDirectory::open(runtime.path())
        .unwrap()
        .unwrap();
    let manifest = ryeos_state::observe_external_content_tree_exact(&runtime).unwrap();
    let runtime_hash = ryeos_state::external_content_manifest_digest(&manifest).unwrap();
    let base_db =
        ryeos_state::StateDb::open(base_store.path(), Arc::new(ryeos_state::TrustStore::new()))
            .unwrap();
    let base_state = base_db.pinned_authority().unwrap();
    let base_guard = base_state.acquire_shared_guard().unwrap();
    let base_cas = base_state.cas_store().unwrap();
    let policy = ProjectSnapshotPolicy::new(
        ryeos_state::project_sync::ProjectSyncScope::FullProject,
        vec![],
        vec![],
        Default::default(),
    )
    .unwrap();
    let policy_hash = base_cas.store_object(&policy.to_value()).unwrap();
    let tree_hash = base_cas
        .store_object(
            &ProjectTree {
                files: Default::default(),
            }
            .to_value(),
        )
        .unwrap();
    let base_snapshot_hash = base_cas
        .store_object(
            &ProjectSnapshot {
                project_tree_hash: tree_hash,
                effective_policy_hash: policy_hash,
                parent_hashes: vec![],
                created_at: "2026-09-21T00:00:00Z".into(),
                message: None,
                source: "external-activation-test".into(),
            }
            .to_value(),
        )
        .unwrap();
    let base_transfer = ryeos_project_capture::prepare_project_snapshot_transfer(
        &base_state,
        &base_guard,
        &base_snapshot_hash,
    )
    .unwrap();
    let base_measurement = base_transfer.measurement();
    let base_authority = base_transfer.descriptor();
    drop(base_cas);
    drop(base_guard);
    drop(base_state);

    let credential = b"synthetic-secret";
    let settings_value = serde_json::json!({
        "expected_credential_sha256": lillux::sha256_hex(credential),
        "faults": [fault],
        "maximum_copy_depth": 8,
        "maximum_copy_entries": 1000,
        "schema": 1,
        "startup_timeout_ms": 2000,
        "state_root": state.path(),
    });
    let settings = lillux::canonical_json(&settings_value)
        .unwrap()
        .into_bytes();
    let common = |operation_id: &str| LifecycleOperationCommon {
        schema: 1,
        protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
        operation_id: operation_id.into(),
        binding_hash: "b".repeat(64),
        settings_digest: lillux::sha256_hex(&settings),
    };
    let reservation = AllocationReservation {
        placement_thread_id: "T-synthetic-activation".into(),
        admitted_capsule_hash: "c".repeat(64),
        base_snapshot_hash: base_snapshot_hash.clone(),
        request_digest: "a".repeat(64),
        maximum_lifetime_seconds: 60,
        contact_deadline_ms: 1,
    };
    let allocate = LifecycleAdapterRequest::Allocate {
        common: common("allocate"),
        reservation: reservation.clone(),
    };
    let allocated: LifecycleAdapterResponse = from_json_slice_strict(
        &invoke_operation(
            &adapter,
            &supervisor,
            &launcher,
            &settings,
            credential,
            &allocate,
            None,
            Vec::new(),
        ),
        MAX_LIFECYCLE_RESPONSE_BYTES,
    )
    .unwrap();
    let LifecycleAdapterResponse::AllocationBound { occurrence_id, .. } = allocated else {
        panic!("synthetic allocation did not bind")
    };
    let occurrence = BoundOccurrence {
        request_digest: reservation.request_digest.clone(),
        occurrence_id,
    };

    let runtime_authority = runtime.inherited_descriptor_authority().unwrap();
    let manifest_bytes = lillux::canonical_json(&serde_json::to_value(&manifest).unwrap()).unwrap();
    let manifest_authority =
        lillux::sealed_memfd(c"synthetic-runtime-manifest", manifest_bytes.as_bytes()).unwrap();
    let guest_inputs = ExternalGuestInputProjection {
        schema: ryeos_external_execution_contract::EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
        base_snapshot: GuestBaseSnapshotInput {
            descriptor: base_authority.inherited_descriptor().unwrap(),
            snapshot_hash: reservation.base_snapshot_hash.clone(),
            closure_digest: base_measurement.closure_digest,
            object_count: base_measurement.object_count,
            blob_count: base_measurement.blob_count,
            total_bytes: base_measurement.total_bytes,
        },
        workspace_outputs: None,
        inputs: vec![GuestMountInput {
            role: GuestMountRole::Product,
            authority_id: "runtime".into(),
            descriptor: runtime_authority.inherited_descriptor().unwrap(),
            destination: "/runtime".into(),
            kind: GuestMountKind::Directory,
            access: GuestMountAccess::ReadOnly,
            normalized_mode: None,
            content_authority:
                ryeos_external_execution_contract::GuestMountContentAuthority::ProductManifest {
                    manifest_kind:
                        ryeos_external_execution_contract::GuestProductManifestKind::Content,
                    manifest_hash: runtime_hash.clone(),
                    manifest_descriptor: manifest_authority.inherited_descriptor().unwrap(),
                    manifest_bytes: manifest_bytes.len() as u64,
                },
            bytes: manifest.total_bytes,
        }],
        executable_search: vec!["/runtime/bin".into()],
        environment: BTreeMap::new(),
    };
    let recipe = ExternalCandidateRuntimeRecipe {
        schema: 2,
        runtime_mount_destination: "/runtime".into(),
        executable_relative_path: "bin/codex".into(),
        argv0: "codex".into(),
        arguments: vec!["exec-server".into(), "--listen".into(), "stdio".into()],
        cwd: "/workspace".into(),
        environment: BTreeMap::new(),
        max_stdout_bytes: 1024 * 1024,
        max_stderr_bytes: 1024 * 1024,
        proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
        contain_process_group: false,
        nested_sandbox: true,
    };
    let controller_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let controller_port = controller_listener.local_addr().unwrap().port();
    controller_listener.set_nonblocking(true).unwrap();
    let controller_stop = Arc::new(AtomicBool::new(false));
    let controller_stop_for_thread = Arc::clone(&controller_stop);
    let controller_accepts = Arc::new(AtomicUsize::new(0));
    let controller_accepts_for_thread = Arc::clone(&controller_accepts);
    let controller_blocker = std::thread::spawn(move || {
        // Cover the 30s activation plus the bounded 5s reconciliation calls.
        let deadline = Instant::now() + Duration::from_secs(90);
        while !controller_stop_for_thread.load(Ordering::Acquire) && Instant::now() < deadline {
            match controller_listener.accept() {
                Ok((mut stream, _)) => {
                    controller_accepts_for_thread.fetch_add(1, Ordering::AcqRel);
                    stream
                        .set_read_timeout(Some(Duration::from_millis(50)))
                        .unwrap();
                    let mut bytes = [0_u8; 4096];
                    while !controller_stop_for_thread.load(Ordering::Acquire)
                        && Instant::now() < deadline
                    {
                        match stream.read(&mut bytes) {
                            Ok(0) => break,
                            Ok(_) => {}
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ) => {}
                            Err(_) => break,
                        }
                    }
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("fake controller accept failed: {error}"),
            }
        }
    });
    let roots =
        vec![ryeos_external_candidate_supervisor::test_support::TEST_CA_DER_BASE64.to_owned()];
    let now = lillux::time::timestamp_millis();
    let requirement = ExternalCandidateRequirement {
        schema: 6,
        required_lifecycle_capabilities: Default::default(),
        protocol: PROTOCOL.into(),
        connector_protocol: ryeos_state::external_execution::admission::CONNECTOR_PROTOCOL.into(),
        execution_route: ExternalCandidateExecutionRoute::ConnectorOnly,
        provider_declaration_id: "synthetic-provider".into(),
        provider_configuration_destination: "environments.toml".into(),
        runtime_product_declaration_id: "runtime".into(),
        runtime_recipe: recipe,
    };
    let qualification_use =
        ryeos_state::external_execution::admission::test_support::fixture_qualification_use(
            &requirement,
        )
        .unwrap();
    let bootstrap = ExternalSupervisorBootstrap {
        schema: 7,
        controller: ExternalControllerTransportContract {
            schema: 2,
            https_origin: format!("https://localhost:{controller_port}"),
            route_contract: EXTERNAL_CHANNEL_ROUTE_CONTRACT.into(),
            tls_root_bundle_digest: external_tls_root_bundle_digest(&roots).unwrap(),
            connect_timeout_ms: 1_000,
            request_timeout_ms: 5_000,
            maximum_response_bytes: 64 * 1024,
            network_inputs: ExternalNetworkInputPolicy {
                resolver: ExternalNetworkInputSelection {
                    source: "/etc/resolv.conf".into(),
                    max_bytes: 64 * 1024,
                },
                hosts: ExternalNetworkInputSelection {
                    source: "/etc/hosts".into(),
                    max_bytes: 64 * 1024,
                },
            },
        },
        tls_root_certificates_der_base64: roots,
        placement_thread_id: reservation.placement_thread_id.clone(),
        occurrence_id: occurrence.occurrence_id.clone(),
        allocation_request_digest: reservation.request_digest.clone(),
        admitted_capsule_hash: reservation.admitted_capsule_hash.clone(),
        base_snapshot_hash: reservation.base_snapshot_hash.clone(),
        execution_binding_hash: "b".repeat(64),
        supervisor_runtime_hash: runtime_hash.clone(),
        launcher_artifact_hash: launcher_hash.clone(),
        candidate_program: AdmittedExternalCandidateProgram {
            runtime_recipe_digest: requirement.runtime_recipe.digest().unwrap(),
            requirement,
            qualification_use,
            runtime_manifest_kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.into(),
            runtime_manifest_hash: runtime_hash.clone(),
            runtime_witness_hash: "1".repeat(64),
            qualification_attestation_hash: "2".repeat(64),
            selection_identity_digest: "3".repeat(64),
        }
        .into(),
        guest_input_identity: guest_inputs.identity_digest().unwrap(),
        guest_inputs: guest_inputs.clone(),
        owner_public_key: ryeos_state::external_execution::encode_channel_public_key(
            &lillux::crypto::SigningKey::from_bytes(&[51; 32]).verifying_key(),
        )
        .unwrap(),
        bootstrap_capability: STANDARD.encode([7_u8; 32]),
        attachment_deadline_ms: now + 60_000,
        execution_timeout_seconds: 1,
        post_execution_timeout_seconds: 1,
        candidate_export_max_bytes: 1024,
        channel_max_bytes: 1024 * 1024,
    };
    let activation = SupervisorActivationIntent {
        activation_request_digest: "6".repeat(64),
        supervisor_runtime_hash: runtime_hash,
        launcher_artifact_hash: launcher_hash,
        attachment_deadline_ms: bootstrap.attachment_deadline_ms,
        execution_timeout_seconds: bootstrap.execution_timeout_seconds,
        post_execution_timeout_seconds: bootstrap.post_execution_timeout_seconds,
        channel_max_bytes: bootstrap.channel_max_bytes,
    };
    let bootstrap_bytes = bootstrap.canonical_bytes().unwrap();
    let bootstrap_handle =
        lillux::sealed_memfd(c"synthetic-package-bootstrap", &bootstrap_bytes).unwrap();
    let package_inputs = ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority::new(
        guest_inputs.clone(),
        base_authority.clone(),
        None,
        vec![runtime_authority.clone()],
        vec![manifest_authority.clone()],
        Vec::new(),
    )
    .unwrap();
    let package_root = lillux::PinnedDirectory::open(state.path())
        .unwrap()
        .unwrap();
    let package_parent = package_root
        .open_or_create_child(std::ffi::OsStr::new("prepared-packages"), 0o700)
        .unwrap();
    let bootstrap_hash = lillux::sha256_hex(&bootstrap_bytes);
    let package_expected =
        ryeos_external_execution_contract::staging_package::GuestStagingExpected {
            inputs: &guest_inputs,
            activation_request_digest: &activation.activation_request_digest,
            bootstrap_sha256: &bootstrap_hash,
            supervisor_sha256: &supervisor_hash,
            launcher_sha256: &activation.launcher_artifact_hash,
            maximum_regular_bytes: 4 * 1024 * 1024 * 1024,
            maximum_framed_bytes: 4 * 1024 * 1024 * 1024,
        };
    let package = ryeos_external_execution::guest_package_producer::prepare_private_guest_package(
        &package_parent,
        &package_inputs,
        &bootstrap_handle,
        &supervisor,
        &launcher,
        &package_expected,
        lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(120)),
    )
    .unwrap();
    let package_authority = package.delivery_descriptor().unwrap();
    let activate = LifecycleAdapterRequest::ActivateSupervisor {
        common: common("activate"),
        occurrence: occurrence.clone(),
        activation: activation.clone(),
        guest_input_identity: guest_inputs.identity_digest().unwrap(),
        guest_input_projection: guest_inputs.clone(),
        import_ticket: ryeos_external_execution_contract::staging_package::GuestImportTicket {
            schema: ryeos_external_execution_contract::staging_package::GUEST_IMPORT_TICKET_SCHEMA,
            binding_hash: "b".repeat(64),
            allocation_request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            activation_request_digest: activation.activation_request_digest.clone(),
            guest_input_identity: guest_inputs.identity_digest().unwrap(),
            payload_sha256: package.sha256().to_owned(),
            manifest_sha256: package.manifest_sha256().to_owned(),
            framed_bytes: package.bytes(),
            regular_bytes: package.manifest().total_regular_bytes,
            bootstrap_sha256: bootstrap_hash.clone(),
            supervisor_sha256: supervisor_hash.clone(),
            launcher_sha256: activation.launcher_artifact_hash.clone(),
            maximum_regular_bytes: 4 * 1024 * 1024 * 1024,
            maximum_framed_bytes: 4 * 1024 * 1024 * 1024,
        },
        guest_package: ryeos_external_execution_contract::LifecycleGuestPackageDelivery {
            descriptor: package_authority.inherited_descriptor().unwrap(),
            payload_sha256: package.sha256().to_owned(),
            manifest_sha256: package.manifest_sha256().to_owned(),
            regular_bytes: package.manifest().total_regular_bytes,
            framed_bytes: package.bytes(),
        },
    };
    if fault == "lose_first_activation_response" {
        // Descriptor relocation leaves the semantic digest intact, but this
        // activation must consume the exact projection handed to the adapter.
        let mut displaced = activate.clone();
        if let LifecycleAdapterRequest::ActivateSupervisor {
            guest_input_projection,
            ..
        } = &mut displaced
        {
            guest_input_projection.base_snapshot.descriptor += 100;
        }
        displaced.validate().unwrap();
        let refused = try_invoke_operation(
            &adapter,
            &supervisor,
            &launcher,
            &settings,
            credential,
            &displaced,
            Some(&bootstrap),
            vec![package_authority.clone()],
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(5)),
        )
        .unwrap_err();
        assert!(
            refused.to_string().contains("unsuccessful_exit"),
            "{refused:#}"
        );
        assert!(
            !state
                .path()
                .join(&occurrence.occurrence_id)
                .join("activation.json")
                .exists(),
            "displaced input authority crossed the activation intent boundary"
        );
    }
    // This fixture stages unstripped debug supervisor/launcher binaries. It
    // hashes and reads over a gigabyte before the SpawnIntent fault boundary.
    // Give that exact operation a bounded harness allowance; production signed
    // startup/contact budgets and the expected fault/phase remain unchanged.
    let error = try_invoke_operation(
        &adapter,
        &supervisor,
        &launcher,
        &settings,
        credential,
        &activate,
        Some(&bootstrap),
        vec![package_authority],
        lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
    )
    .unwrap_err();
    package.discard().unwrap();
    assert!(
        error
            .to_string()
            .contains("external lifecycle adapter failed: unsuccessful_exit"),
        "{error:#}"
    );

    let reconcile = LifecycleAdapterRequest::ReconcileSupervisorActivation {
        common: common("activate"),
        occurrence: occurrence.clone(),
        activation: activation.clone(),
    };
    if matches!(
        fault,
        "fail_after_staging_intent" | "fail_after_spawn_intent"
    ) {
        let invoke_fresh = |request: &LifecycleAdapterRequest| {
            let response: LifecycleAdapterResponse = from_json_slice_strict(
                &invoke_operation(
                    &adapter,
                    &supervisor,
                    &launcher,
                    &settings,
                    credential,
                    request,
                    None,
                    Vec::new(),
                ),
                MAX_LIFECYCLE_RESPONSE_BYTES,
            )
            .unwrap();
            response.validate_for(request).unwrap();
            response
        };
        let occurrence_path = state.path().join(&occurrence.occurrence_id);
        let before: serde_json::Value = serde_json::from_slice(
            &std::fs::read(occurrence_path.join("activation.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(before["schema"], 2);
        assert_eq!(
            before["activation_request_digest"],
            activation.activation_request_digest
        );
        assert_eq!(before["occurrence_id"], occurrence.occurrence_id);
        let can_fence = fault == "fail_after_staging_intent";
        assert_eq!(
            before["phase"],
            if can_fence { "staging" } else { "spawn_intent" }
        );
        assert!(matches!(
            invoke_fresh(&reconcile),
            LifecycleAdapterResponse::SupervisorPending { .. }
        ));
        let termination = TerminationIntent {
            termination_request_digest: "7".repeat(64),
        };
        let terminate = LifecycleAdapterRequest::Terminate {
            common: common("terminate"),
            occurrence: occurrence.clone(),
            termination: termination.clone(),
        };
        let terminal = invoke_fresh(&terminate);
        let reconcile_termination = LifecycleAdapterRequest::ReconcileTermination {
            common: common("terminate"),
            occurrence: occurrence.clone(),
            termination,
        };
        let repeated = invoke_fresh(&reconcile_termination);
        assert_eq!(
            serde_json::to_value(&terminal).unwrap(),
            serde_json::to_value(&repeated).unwrap()
        );
        let after: serde_json::Value = serde_json::from_slice(
            &std::fs::read(occurrence_path.join("activation.json")).unwrap(),
        )
        .unwrap();
        let mut expected = before;
        if can_fence {
            expected["phase"] = "fenced_before_spawn".into();
            assert!(matches!(
                terminal,
                LifecycleAdapterResponse::OccurrenceTerminal { .. }
            ));
            let fenced = invoke_fresh(&reconcile);
            assert!(matches!(
                fenced,
                LifecycleAdapterResponse::SupervisorNotStarted { .. }
            ));
            // The original activation cannot reopen the fence, even from a
            // new process. No bootstrap is needed to observe the existing cut.
            let late = invoke_fresh(&activate);
            assert_eq!(
                serde_json::to_value(fenced).unwrap(),
                serde_json::to_value(late).unwrap()
            );
        } else {
            assert!(matches!(
                terminal,
                LifecycleAdapterResponse::TerminationPending { .. }
            ));
            assert!(matches!(
                invoke_fresh(&reconcile),
                LifecycleAdapterResponse::SupervisorPending { .. }
            ));
            assert!(matches!(
                invoke_fresh(&activate),
                LifecycleAdapterResponse::SupervisorPending { .. }
            ));
        }
        assert_eq!(
            after, expected,
            "only the positive staging fence may advance"
        );
        let after_late: serde_json::Value = serde_json::from_slice(
            &std::fs::read(occurrence_path.join("activation.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(after_late, expected);
        controller_stop.store(true, Ordering::Release);
        controller_blocker.join().unwrap();
        assert_eq!(controller_accepts.load(Ordering::Acquire), 0);
        // These absences are corroborating observations, NOT terminal proof.
        assert!(!occurrence_path.join("ready.json").exists());
        assert!(!occurrence_path.join("terminal.json").exists());
        return;
    }
    let marker: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            state
                .path()
                .join(&occurrence.occurrence_id)
                .join("fault-activation-response-lost.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(marker["fault"], "lose_first_activation_response");
    assert_eq!(
        marker["request_digest"],
        activation.activation_request_digest
    );
    let started: LifecycleAdapterResponse = from_json_slice_strict(
        &invoke_operation(
            &adapter,
            &supervisor,
            &launcher,
            &settings,
            credential,
            &reconcile,
            None,
            Vec::new(),
        ),
        MAX_LIFECYCLE_RESPONSE_BYTES,
    )
    .unwrap();
    started.validate_for(&reconcile).unwrap();
    assert!(matches!(
        started,
        LifecycleAdapterResponse::SupervisorStarted { .. }
    ));
    let repeated_started: LifecycleAdapterResponse = from_json_slice_strict(
        &invoke_operation(
            &adapter,
            &supervisor,
            &launcher,
            &settings,
            credential,
            &reconcile,
            None,
            Vec::new(),
        ),
        MAX_LIFECYCLE_RESPONSE_BYTES,
    )
    .unwrap();
    repeated_started.validate_for(&reconcile).unwrap();
    assert_eq!(
        lillux::canonical_json(&serde_json::to_value(&started).unwrap()).unwrap(),
        lillux::canonical_json(&serde_json::to_value(&repeated_started).unwrap()).unwrap(),
        "an exact activation reconciliation changed the original supervisor observation"
    );

    let termination = TerminationIntent {
        termination_request_digest: "7".repeat(64),
    };
    let terminate = LifecycleAdapterRequest::Terminate {
        common: common("terminate"),
        occurrence,
        termination,
    };
    let terminal: LifecycleAdapterResponse = from_json_slice_strict(
        &invoke_operation(
            &adapter,
            &supervisor,
            &launcher,
            &settings,
            credential,
            &terminate,
            None,
            Vec::new(),
        ),
        MAX_LIFECYCLE_RESPONSE_BYTES,
    )
    .unwrap();
    terminal.validate_for(&terminate).unwrap();
    assert!(matches!(
        terminal,
        LifecycleAdapterResponse::OccurrenceTerminal { .. }
    ));
    let repeated_terminal: LifecycleAdapterResponse = from_json_slice_strict(
        &invoke_operation(
            &adapter,
            &supervisor,
            &launcher,
            &settings,
            credential,
            &terminate,
            None,
            Vec::new(),
        ),
        MAX_LIFECYCLE_RESPONSE_BYTES,
    )
    .unwrap();
    repeated_terminal.validate_for(&terminate).unwrap();
    assert_eq!(
        lillux::canonical_json(&serde_json::to_value(&terminal).unwrap()).unwrap(),
        lillux::canonical_json(&serde_json::to_value(&repeated_terminal).unwrap()).unwrap(),
        "an exact terminal retry changed the original occurrence observation"
    );
    controller_stop.store(true, Ordering::Release);
    controller_blocker.join().unwrap();
}
