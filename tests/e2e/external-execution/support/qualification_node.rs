//! Test-node bootstrap for the synthetic runtime qualification fixtures.
//!
//! These helpers build the same node an end-to-end qualification needs: a
//! signed authoring bundle registered in a live engine, the daemon execution
//! isolation, the node execution identity and a granted local operator. They
//! author no qualification evidence themselves; the fixtures that call them
//! publish evidence only through the production qualification owners.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD};

/// A node policy snapshot whose every section is the shipped core profile's,
/// compiled by the production section parsers, except the sections `fixed`
/// supplies: those the fixture sizes for its own runtimes.
pub fn node_policy_snapshot(
    app_root: &Path,
    signer_fingerprint: &str,
    fixed: Vec<Arc<dyn ryeos_node_runtime::node_policy::ErasedNodePolicy>>,
) -> ryeos_node_runtime::node_policy::NodePolicySnapshot {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .unwrap();
    let profile =
        std::fs::read_to_string(repository.join("bundles/.ai/node/init/profiles/core.yaml"))
            .unwrap();
    let profile: serde_json::Value = serde_yaml::from_str(&profile).unwrap();
    let fixed_names = fixed
        .iter()
        .map(|record| record.section_name())
        .collect::<std::collections::BTreeSet<_>>();
    let mut records = fixed;
    let table = ryeos_node_runtime::node_policy::NodePolicyTable::new();
    for section in table.sections() {
        let name = section.name();
        if fixed_names.contains(name) {
            continue;
        }
        let context = ryeos_node_runtime::node_policy::NodePolicyContext {
            section: name.to_owned(),
            source_file: app_root.join(format!(".ai/node/policy/{name}.yaml")),
            signer_fingerprint: signer_fingerprint.to_owned(),
        };
        // The profile supports several host classes; a node runs under the
        // one its deployment selected. This fixture is a local host.
        let body = if name == "isolation" {
            ryeos_node_runtime::node_policy::sections::isolation::select_host_class(
                &profile["policies"][name],
                "local",
            )
            .unwrap()
        } else {
            profile["policies"][name].clone()
        };
        records.push(
            section
                .parse(&context, &body)
                .unwrap_or_else(|error| panic!("compile shipped `{name}` policy: {error:#}")),
        );
    }
    ryeos_node_runtime::node_policy::NodePolicySnapshot::from_test_records(records)
}

pub fn manifest_hash(
    manifest: &ryeos_state::objects::ExternalLargeContentManifestObject,
) -> String {
    lillux::sha256_hex(
        lillux::canonical_json(&manifest.to_value().unwrap())
            .unwrap()
            .as_bytes(),
    )
}

pub(super) struct ScratchSink {
    pub(super) cas: lillux::CasStore,
    pub(super) large: ryeos_state::LargeObjectStore,
}

impl ryeos_state::ExternalLargeContentSink for ScratchSink {
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

pub(super) fn large_capture_policy(
    ignore: &ryeos_state::ignore::IgnoreMatcher,
) -> ryeos_state::LargeContentCapturePolicy<'_> {
    ryeos_state::LargeContentCapturePolicy::new(
        "runtime".into(),
        ignore,
        ryeos_state::LargeContentCaptureBounds {
            max_depth: 8,
            max_entries: 16,
            max_file_bytes: 512 * 1024 * 1024,
            max_total_bytes: 1024 * 1024 * 1024,
        },
    )
    .unwrap()
}

pub(super) fn no_ignore() -> ryeos_state::ignore::IgnoreMatcher {
    ryeos_state::ignore::IgnoreMatcher::from_config(&ryeos_state::ignore::IgnoreConfig {
        patterns: Vec::new(),
    })
    .unwrap()
}

/// The manifest the production large-content capture derives for a tree,
/// computed in a throwaway store so no node holds the bytes yet. An activation
/// recipe's signed digest must name this identity before acquisition runs.
pub fn predict_large_manifest(
    runtime: &Path,
) -> ryeos_state::objects::ExternalLargeContentManifestObject {
    let scratch = tempfile::tempdir().unwrap();
    let directory = lillux::PinnedDirectory::open(scratch.path())
        .unwrap()
        .unwrap();
    directory.tighten_owner_private_directory().unwrap();
    let large = ryeos_state::LargeObjectStore::open_or_create_under(&directory).unwrap();
    let mut sink = ScratchSink {
        cas: lillux::CasStore::new(scratch.path().join("cas")),
        large,
    };
    let runtime = lillux::PinnedDirectory::open(runtime).unwrap().unwrap();
    let ignore = no_ignore();
    ryeos_state::capture_large_tree(&runtime, &large_capture_policy(&ignore), &mut sink).unwrap()
}

/// Write the node-signed grant the local operator needs before any
/// operator-owned qualification step may run.
pub fn install_operator_grant(state: &ryeos_app::state::AppState) {
    let operator =
        ryeos_node_runtime::identity::NodeIdentity::load(&state.config.operator_signing_key_path)
            .unwrap();
    let scopes = vec!["*".to_owned()];
    ryeos_node_runtime::identity::write_authorized_key_toml(
        &state.config.authorized_keys_dir,
        operator.fingerprint(),
        &STANDARD.encode(operator.verifying_key().as_bytes()),
        &scopes,
        "synthetic runtime qualification test operator",
        state.identity.fingerprint(),
        "2026-09-22T00:00:00Z",
        state.identity.signing_key(),
        ryeos_node_runtime::identity::WildcardPolicy::AllowBootstrap,
    )
    .unwrap();
    ryeos_node_runtime::identity::load_verified_authorized_key(
        operator.fingerprint(),
        &state.config.authorized_keys_dir,
        &state.identity,
    )
    .unwrap()
    .unwrap();
}

/// The operator context the qualification handlers authenticate.
pub fn operator_context(
    state: &ryeos_app::state::AppState,
) -> ryeos_app::handler_context::HandlerContext {
    let operator =
        ryeos_node_runtime::identity::NodeIdentity::load(&state.config.operator_signing_key_path)
            .unwrap();
    ryeos_app::handler_context::HandlerContext::new_with_authority(
        operator.principal_id(),
        vec!["*".into()],
        true,
        Some(ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass::LocalClient),
        None,
    )
}

pub fn daemon_isolation_roots() -> Vec<PathBuf> {
    let mut roots = vec![ryeos_engine::test_support::core_bundle_root()];
    roots.extend(ryeos_engine::test_support::workflow_bundle_roots());
    roots
}

/// Enforced descriptor-bound realization mounts, as qualification verifiers
/// require; trusted process-group sessions are the explicit fixture contract.
pub fn composed_isolation_policy() -> ryeos_engine::isolation::IsolationPolicy {
    let mut policy = ryeos_engine::isolation::IsolationPolicy::disabled_for_authoring();
    policy.mode = ryeos_engine::isolation::IsolationMode::Enforce;
    policy.backend = Some(ryeos_isolation_protocol::IsolationBackendSelection {
        bundle: "core".into(),
        implementation: "linux-lillux".into(),
    });
    policy.filesystem.proc_filesystem =
        ryeos_isolation_protocol::IsolationProcFilesystem::PidNamespace;
    policy.trusted_process_group_sessions = true;
    policy
}

struct BundleGenerationLifeline;

impl ryeos_engine::isolation::IsolationGenerationLifeline for BundleGenerationLifeline {
    fn begin_operation(&self) -> Result<Box<dyn Send + Sync>, String> {
        Ok(Box::new(()))
    }

    fn ensure_current(&self) -> Result<(), String> {
        Ok(())
    }
}

fn live_bundle_engine(
    authoring_bundle: PathBuf,
    bundle_name: &str,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
    isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
) -> ryeos_engine::engine::Engine {
    let mut trust = ryeos_engine::test_support::live_trust_store();
    trust.extend_from(&ryeos_engine::trust::TrustStore::from_signers(vec![
        ryeos_engine::trust::TrustedSigner {
            fingerprint: identity.fingerprint().to_owned(),
            verifying_key: *identity.verifying_key(),
            label: Some("synthetic runtime qualification authoring bundle".into()),
        },
    ]));
    let core = ryeos_engine::test_support::core_bundle_root();
    let workflow = ryeos_engine::test_support::workflow_bundle_roots();
    let mut kind_roots = vec![core.join(".ai/node/engine/kinds")];
    kind_roots.extend(
        workflow
            .iter()
            .map(|root| root.join(".ai/node/engine/kinds")),
    );
    let kinds = ryeos_engine::kind_registry::KindRegistry::load_base(&kind_roots, &trust).unwrap();
    let mut roots = vec![core];
    roots.extend(workflow);
    roots.push(authoring_bundle);
    let tagged_roots = roots
        .iter()
        .cloned()
        .map(|root| (root, ryeos_engine::resolution::TrustClass::TrustedBundle))
        .collect::<Vec<_>>();
    let registered_roots: Vec<_> = ["core", "knowledge", "directive", "graph", bundle_name]
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
        Arc::new(BundleGenerationLifeline),
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

/// Node-owned acquisition policy a fixture grants. `None` leaves managed
/// activation disabled, as the captured-product qualification does.
pub struct ManagedActivationGrant {
    pub archive_root_name: String,
    pub archive_root: PathBuf,
    pub limits:
        ryeos_node_runtime::node_policy::sections::external_content::ManagedExternalContentActivationPolicy,
}

fn install_bundle_engine(
    state: &mut ryeos_app::state::AppState,
    authoring_bundle: PathBuf,
    bundle_name: &str,
    isolation_policy: ryeos_engine::isolation::IsolationPolicy,
    activation: Option<&ManagedActivationGrant>,
) {
    use ryeos_node_runtime::node_policy::sections::external_content::{
        ExternalContentImportLimits, ExternalContentImportPolicyRecord, ExternalContentImportRoot,
        ManagedExternalContentPolicy,
    };

    let mut roots = BTreeMap::new();
    let managed_activation = match activation {
        Some(grant) => {
            let root = lillux::PinnedDirectory::open(&grant.archive_root)
                .unwrap()
                .unwrap();
            let root_identity = root.identity().unwrap();
            roots.insert(
                grant.archive_root_name.clone(),
                ExternalContentImportRoot {
                    path: grant.archive_root.clone(),
                    root_identity,
                },
            );
            ManagedExternalContentPolicy {
                enabled: true,
                limits: Some(grant.limits.clone()),
            }
        }
        None => ManagedExternalContentPolicy {
            enabled: false,
            limits: None,
        },
    };
    let import_policy = ExternalContentImportPolicyRecord {
        schema: 1,
        roots,
        declared_roots: std::collections::BTreeMap::new(),
        limits: ExternalContentImportLimits {
            max_depth: 8,
            max_entries: 16,
            max_file_bytes: 512 * 1024 * 1024,
            max_total_bytes: 1024 * 1024 * 1024,
            store_budget_bytes: 2 * 1024 * 1024 * 1024,
            minimum_free_bytes: 64 * 1024 * 1024,
        },
        managed_activation,
    };
    import_policy.validate().unwrap();
    let mut isolation_roots = vec![ryeos_engine::test_support::core_bundle_root()];
    isolation_roots.extend(ryeos_engine::test_support::workflow_bundle_roots());
    isolation_roots.push(authoring_bundle.clone());
    let trust = ryeos_engine::test_support::live_trust_store();
    let isolation = ryeos_node_runtime::engine_init::load_test_execution_isolation(
        &state.config.app_root,
        &isolation_roots,
        &trust,
        isolation_policy,
    )
    .unwrap();
    let engine = Arc::new(live_bundle_engine(
        authoring_bundle,
        bundle_name,
        &state.identity,
        Arc::clone(&isolation),
    ));
    state.isolation = isolation;
    let mut kind_roots =
        vec![ryeos_engine::test_support::core_bundle_root().join(".ai/node/engine/kinds")];
    kind_roots.extend(
        ryeos_engine::test_support::workflow_bundle_roots()
            .iter()
            .map(|root| root.join(".ai/node/engine/kinds")),
    );
    let kinds = ryeos_engine::kind_registry::KindRegistry::load_base(&kind_roots, &trust).unwrap();
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
    state.node_policy = Arc::new(node_policy_snapshot(
        &state.config.app_root,
        state.identity.fingerprint(),
        vec![
            Arc::new(ryeos_engine::history_policy::ResolvedNodeThreadHistoryPolicy::test_policy()),
            Arc::new(import_policy),
            Arc::new(
                ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy {
                    schema: 1,
                    max_roots: 256,
                    max_objects: 32_768,
                    max_blobs: 32_768,
                    max_object_bytes: 32 * 1024 * 1024,
                    max_total_object_bytes: 64 * 1024 * 1024,
                    max_blob_bytes: 128 * 1024 * 1024,
                    max_total_blob_bytes: 640 * 1024 * 1024,
                    max_response_bytes: 1024 * 1024 * 1024,
                    max_links_per_object: 100_000,
                    local_verification: None,
                },
            ),
            ],
    ));
}

/// Bring a freshly built test node to the point where its signed authoring
/// bundle is live: operator granted, bundle engine and node policy installed,
/// daemon isolation and execution identity booted. The authoring bundle's
/// signed items must be written before this runs. The returned listener owns
/// the daemon socket path and must outlive every qualification step.
pub fn start_bundle_node(
    state: &mut ryeos_app::state::AppState,
    authoring_bundle: &Path,
    bundle_name: &str,
    activation: Option<&ManagedActivationGrant>,
) -> tokio::net::UnixListener {
    install_operator_grant(state);
    state.started_at_iso = lillux::time::iso8601_now();
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
    let listener = tokio::net::UnixListener::bind(&state.config.uds_path).unwrap();
    let isolation_policy = composed_isolation_policy();
    install_bundle_engine(
        state,
        authoring_bundle.to_owned(),
        bundle_name,
        isolation_policy.clone(),
        activation,
    );
    let trust = ryeos_engine::test_support::live_trust_store();
    let app_root = state.config.app_root.clone();
    state.isolation = ryeos_node_runtime::engine_init::load_test_daemon_execution_isolation(
        &app_root,
        &state.config.uds_path,
        &daemon_isolation_roots(),
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
    let extensions = (*state.extensions).clone();
    extensions.insert(execution_identity);
    state.extensions = Arc::new(extensions);
    listener
}

/// Authorize exact pinned manifests for one signed verifier tool, as its
/// admitted consumer. A subject and its independent verifier runtime may share
/// an identity; one active binding authorizes it, a second would conflict.
pub fn bind_manifests_to_tool(
    state: &ryeos_app::state::AppState,
    tool_ref: &str,
    manifest_hashes: &[&str],
) {
    let manifests = manifest_hashes
        .iter()
        .map(|hash| {
            (
                (*hash).to_owned(),
                ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.to_owned(),
            )
        })
        .collect::<Vec<_>>();
    bind_typed_manifests_to_tool(state, tool_ref, &manifests);
}

/// As [`bind_manifests_to_tool`], for manifests of differing kinds: each entry
/// is a manifest hash with its object kind.
pub fn bind_typed_manifests_to_tool(
    state: &ryeos_app::state::AppState,
    tool_ref: &str,
    manifests: &[(String, String)],
) {
    use ryeos_engine::contracts::SubjectResolutionAuthority;
    use std::collections::BTreeMap as Manifests;

    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(tool_ref).unwrap();
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
        ryeos_node_runtime::identity::NodeIdentity::load(&state.config.operator_signing_key_path)
            .unwrap();
    let operator_grant = ryeos_node_runtime::identity::load_verified_authorized_key(
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
    let manifests = manifests
        .iter()
        .map(|(hash, kind)| (hash.as_str(), kind.as_str()))
        .collect::<Manifests<_, _>>();
    for (manifest_hash, manifest_kind) in manifests {
        let binding = ryeos_state::objects::ExternalContentBinding::active(
            manifest_hash.to_owned(),
            manifest_kind.to_owned(),
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

/// A nested test runtime must not be synchronously dropped from an async
/// Tokio worker. Its accepted dispatch task stays owned until this guard
/// leaves the surrounding fixture, including on a test panic.
pub struct RetainedTestRuntime(pub Option<tokio::runtime::Runtime>);

impl Drop for RetainedTestRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}
