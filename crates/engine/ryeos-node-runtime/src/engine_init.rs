//! Engine initialization for ryeosd (Model B).
//!
//! Constructs a `ryeos_engine::engine::Engine` at daemon startup using
//! only registered bundle roots — NOT the app_root itself.
//! Model B: bundles live under `<app_root>/.ai/bundles/{name}/`,
//! each registered at `<app_root>/.ai/node/bundles/{name}.yaml`.
//! The engine crate is kind-agnostic — all kind definitions come from
//! `*.kind-schema.yaml` files found under `{AI_DIR}/node/engine/kinds/`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};

use ryeos_engine::boot_validation::{
    validate_boot, validate_protocol_builder, validate_runtime_launch_handlers,
};
use ryeos_engine::composers::ComposerRegistry;
use ryeos_engine::effective_validators::EffectiveValidatorRegistry;
use ryeos_engine::engine::Engine;
use ryeos_engine::handlers::HandlerRegistry;
use ryeos_engine::kind_registry::{KindRegistry, TerminatorDecl};
use ryeos_engine::launch_preparers::{LaunchPreparerRegistry, LaunchPreparerRunner};
use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};
use ryeos_engine::protocols::ProtocolRegistry;
use ryeos_engine::resolution::TrustClass;
use ryeos_engine::runtime_registry::RuntimeRegistry;
use ryeos_engine::trust::TrustStore;

use crate::config::Config;
use crate::node_config_source::BundleRecord;
use ryeos_isolation_protocol::{
    AdapterInspectionRequest, AdapterInspectionResponse, IsolationAdapterProtocolVersion,
};

#[derive(Debug)]
pub struct VerifiedBundleGeneration {
    records: Vec<BundleRecord>,
    bundles: Vec<VerifiedBundleGenerationRecord>,
}

#[derive(Debug)]
struct VerifiedBundleGenerationRecord {
    name: String,
    canonical_root: PathBuf,
    root_authority: Arc<lillux::PinnedDirectory>,
    manifest_body_digest: String,
    manifest_signer_fingerprint: String,
    executor_manifest: Option<ryeos_engine::binary_resolver::BundleExecutorManifestIdentity>,
}

impl VerifiedBundleGeneration {
    pub fn capture(records: Vec<BundleRecord>, node_trust_store: &TrustStore) -> Result<Self> {
        let mut bundles = Vec::with_capacity(records.len());
        for record in &records {
            let canonical_root = std::fs::canonicalize(&record.path)
                .with_context(|| format!("canonicalize bundle root {}", record.path.display()))?;
            if canonical_root != record.path {
                anyhow::bail!(
                    "bundle '{}' registration is not canonical: {} resolved to {}",
                    record.name,
                    record.path.display(),
                    canonical_root.display()
                );
            }
            let root_authority = lillux::PinnedDirectory::open(&canonical_root)
                .with_context(|| format!("pin bundle root {}", canonical_root.display()))?
                .with_context(|| {
                    format!("bundle root disappeared: {}", canonical_root.display())
                })?;
            let manifest = ryeos_engine::plan_builder::verify_bundle_source_manifest_identity(
                &canonical_root,
                &record.name,
                node_trust_store,
            )
            .with_context(|| format!("verify bundle manifest for '{}'", record.name))?;
            let executor_manifest =
                ryeos_engine::binary_resolver::verify_bundle_executor_manifest_identity(
                    &canonical_root,
                    node_trust_store,
                )
                .with_context(|| {
                    format!("verify executor manifest identity for '{}'", record.name)
                })?;
            bundles.push(VerifiedBundleGenerationRecord {
                name: record.name.clone(),
                canonical_root,
                root_authority: Arc::new(root_authority),
                manifest_body_digest: manifest.body_digest,
                manifest_signer_fingerprint: manifest.signer_fingerprint,
                executor_manifest,
            });
        }

        let generation = Self { records, bundles };
        generation.ensure_current(node_trust_store)?;
        Ok(generation)
    }

    pub fn records(&self) -> &[BundleRecord] {
        &self.records
    }

    pub fn roots(&self) -> Vec<PathBuf> {
        self.bundles
            .iter()
            .map(|bundle| bundle.canonical_root.clone())
            .collect()
    }

    pub fn registered_roots(&self) -> Vec<ryeos_engine::item_resolution::RegisteredBundleRoot> {
        self.bundles
            .iter()
            .map(
                |bundle| ryeos_engine::item_resolution::RegisteredBundleRoot {
                    name: bundle.name.clone(),
                    canonical_root: bundle.canonical_root.clone(),
                },
            )
            .collect()
    }

    pub fn ensure_current(&self, node_trust_store: &TrustStore) -> Result<()> {
        if self.records.len() != self.bundles.len() {
            anyhow::bail!("verified bundle generation has inconsistent record counts");
        }
        for bundle in &self.bundles {
            ensure_generation_root_binding(bundle)?;
            let manifest = ryeos_engine::plan_builder::verify_bundle_source_manifest_identity(
                &bundle.canonical_root,
                &bundle.name,
                node_trust_store,
            )
            .with_context(|| format!("re-verify bundle manifest for '{}'", bundle.name))?;
            if manifest.body_digest != bundle.manifest_body_digest
                || manifest.signer_fingerprint != bundle.manifest_signer_fingerprint
            {
                anyhow::bail!(
                    "bundle '{}' manifest identity changed within one node generation",
                    bundle.name
                );
            }
            let executor_manifest =
                ryeos_engine::binary_resolver::verify_bundle_executor_manifest_ref_identity(
                    &bundle.canonical_root,
                    node_trust_store,
                )
                .with_context(|| {
                    format!("re-verify executor manifest identity for '{}'", bundle.name)
                })?;
            if executor_manifest != bundle.executor_manifest {
                anyhow::bail!(
                    "bundle '{}' executor-manifest identity changed within one node generation",
                    bundle.name
                );
            }
            ensure_generation_root_binding(bundle)?;
        }
        Ok(())
    }

    pub fn checked<T>(
        &self,
        node_trust_store: &TrustStore,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.ensure_current(node_trust_store)?;
        let value = operation()?;
        self.ensure_current(node_trust_store)?;
        Ok(value)
    }
}

fn ensure_generation_root_binding(bundle: &VerifiedBundleGenerationRecord) -> Result<()> {
    let current = lillux::PinnedDirectory::open(&bundle.canonical_root)
        .with_context(|| format!("reopen bundle root {}", bundle.canonical_root.display()))?
        .with_context(|| {
            format!(
                "bundle '{}' root disappeared within one node generation",
                bundle.name
            )
        })?;
    if !bundle.root_authority.is_same_directory(&current)? {
        anyhow::bail!(
            "bundle '{}' root identity changed within one node generation",
            bundle.name
        );
    }
    Ok(())
}

struct RetainedRegisteredGeneration {
    app_root: PathBuf,
    _read_lock: Option<Arc<crate::registry::BundleRegistryReadLock>>,
    verified_generation: Arc<VerifiedBundleGeneration>,
    node_trust_store: TrustStore,
}

impl ryeos_engine::isolation::IsolationGenerationLifeline for RetainedRegisteredGeneration {
    fn begin_operation(&self) -> std::result::Result<Box<dyn Send + Sync>, String> {
        if self._read_lock.is_some() {
            return Ok(Box::new(()));
        }
        crate::registry::BundleRegistryReadLock::acquire(&self.app_root)
            .map(|guard| Box::new(guard) as Box<dyn Send + Sync>)
            .map_err(|error| format!("{error:#}"))
    }

    fn ensure_current(&self) -> std::result::Result<(), String> {
        self.verified_generation
            .ensure_current(&self.node_trust_store)
            .map_err(|error| format!("{error:#}"))
    }
}

pub fn load_registered_isolation(
    app_root: &std::path::Path,
) -> Result<Arc<ryeos_engine::isolation::IsolationRuntime>> {
    let _generation_lock =
        crate::registry::BundleRegistryMutationLock::acquire_for_startup(app_root)
            .context("acquire bundle-generation lock for isolation composition")?;
    load_registered_generation_under_lock(
        app_root,
        ryeos_engine::isolation::IsolationRuntime::resolve_compiled_policy,
        true,
    )
    .map(|generation| generation.0)
}

/// Standalone composition pins and verifies one generation under a bounded
/// shared lock. Each later verified-open-to-exec operation reacquires a short
/// read lock via the generation lifeline; the returned runtime itself never
/// holds `.bundles.lock`, so a long-lived TUI cannot block node boot or
/// replacement. Shared composition also permits a verified offline tool to
/// compose its own read-only engine while its parent guards the launch.
pub fn load_locked_registered_isolation(
    app_root: &std::path::Path,
) -> Result<Arc<ryeos_engine::isolation::IsolationRuntime>> {
    load_locked_registered_isolation_with(
        app_root,
        ryeos_engine::isolation::IsolationRuntime::resolve_compiled_policy,
        true,
    )
}

/// Verify registered definitions under the same trust, lock and generation
/// lifeline as execution, without acquiring the supervisor's process scopes.
/// Metadata discovery and bundle verification are not execution controllers.
/// This is not an execution-admission fallback: an actual scope-requiring
/// launch must use its execution owner's fully admitted runtime.
pub fn load_locked_registered_definition_isolation(
    app_root: &std::path::Path,
) -> Result<Arc<ryeos_engine::isolation::IsolationRuntime>> {
    load_locked_registered_isolation_with(
        app_root,
        ryeos_engine::isolation::IsolationRuntime::resolve_compiled_policy_for_definition_validation,
        false,
    )
}

type RegisteredIsolationResolver = fn(
    &std::path::Path,
    ryeos_engine::isolation::IsolationPolicy,
    PathBuf,
    String,
    Option<Arc<ryeos_engine::isolation::ResolvedIsolationBackend>>,
) -> std::result::Result<
    ryeos_engine::isolation::IsolationRuntime,
    ryeos_engine::error::EngineError,
>;

fn load_locked_registered_isolation_with(
    app_root: &std::path::Path,
    resolve: RegisteredIsolationResolver,
    retain_captures: bool,
) -> Result<Arc<ryeos_engine::isolation::IsolationRuntime>> {
    let generation_lock =
        crate::registry::BundleRegistryReadLock::acquire_for_composition(app_root)
            .context("acquire bundle-generation read lock for isolation composition")?;
    let (runtime, node_trust, generation) =
        load_registered_generation_under_lock(app_root, resolve, retain_captures)?;
    let bundle_roots = generation.registered_roots();
    let lifeline = Arc::new(RetainedRegisteredGeneration {
        app_root: app_root.to_path_buf(),
        _read_lock: None,
        verified_generation: generation,
        node_trust_store: node_trust.clone(),
    });
    let runtime = Arc::try_unwrap(runtime)
        .map_err(|_| anyhow::anyhow!("new isolation runtime unexpectedly has shared ownership"))?
        .retain_registered_generation(lifeline, node_trust, bundle_roots);
    drop(generation_lock);
    Ok(Arc::new(runtime))
}

fn load_registered_generation_under_lock(
    app_root: &std::path::Path,
    resolve: RegisteredIsolationResolver,
    retain_captures: bool,
) -> Result<(
    Arc<ryeos_engine::isolation::IsolationRuntime>,
    TrustStore,
    Arc<VerifiedBundleGeneration>,
)> {
    let trust_store = TrustStore::load(None, &app_root.join(ryeos_engine::AI_DIR).join("config"))
        .context("load node trust for isolation backend resolution")?;
    let loader = crate::bundle_bootstrap::BundleBootstrapLoader {
        app_root,
        trust_store: &trust_store,
    };
    let records = loader
        .load_bundle_section()
        .context("load bundle registrations for isolation backend resolution")?;
    let generation = Arc::new(VerifiedBundleGeneration::capture(records, &trust_store)?);
    generation
        .checked(&trust_store, || {
            validate_installed_bundle_plan(app_root, &generation, &trust_store)
        })
        .context("admit installed bundle graph for isolation composition")?;
    let policy_table = crate::node_policy::NodePolicyTable::new();
    let policy_generation = crate::node_policy::generation::load_policy_generation(
        app_root,
        &trust_store,
        &policy_table,
    )?;
    let policy_snapshot = crate::node_policy::compile_generation(
        app_root,
        &policy_table,
        &policy_generation,
        &crate::identity::node_identity_fingerprint(app_root)?,
    )?;
    let policy = policy_snapshot
        .require::<ryeos_engine::isolation::IsolationPolicy>()?
        .clone();
    let (backend, verified_backend_capture) = generation.checked(&trust_store, || {
        if retain_captures {
            resolve_isolation_backend_with_verified_captures(&generation, &trust_store, &policy)
        } else {
            resolve_isolation_backend(&generation, &trust_store, &policy)
                .map(|backend| (backend, None))
        }
    })?;
    let runtime = resolve(
        app_root,
        policy,
        crate::node_policy::generation::policy_directory(app_root).join("isolation.yaml"),
        format!("sha256:{}", policy_generation.digest()),
        backend,
    )
    .map_err(anyhow::Error::from)?;
    let runtime = match verified_backend_capture {
        Some(capture) => runtime.retain_verified_backend_capture(capture)?,
        None => runtime,
    };
    generation.ensure_current(&trust_store)?;
    Ok((Arc::new(runtime), trust_store, generation))
}

pub fn resolve_isolation_backend(
    generation: &VerifiedBundleGeneration,
    node_trust_store: &TrustStore,
    policy: &ryeos_engine::isolation::IsolationPolicy,
) -> Result<Option<Arc<ryeos_engine::isolation::ResolvedIsolationBackend>>> {
    resolve_isolation_backend_with_capture_retention(generation, node_trust_store, policy, false)
        .map(|(backend, _)| backend)
}

/// Retain actual selected executable captures as content facts alongside the
/// same resolved backend. The protected caller independently owns node trust,
/// registered generation and policy; this return does not mint that authority.
pub fn resolve_isolation_backend_with_verified_captures(
    generation: &VerifiedBundleGeneration,
    node_trust_store: &TrustStore,
    policy: &ryeos_engine::isolation::IsolationPolicy,
) -> Result<(
    Option<Arc<ryeos_engine::isolation::ResolvedIsolationBackend>>,
    Option<Arc<ryeos_engine::isolation::VerifiedIsolationBackendCapture>>,
)> {
    resolve_isolation_backend_with_capture_retention(generation, node_trust_store, policy, true)
}

fn resolve_isolation_backend_with_capture_retention(
    generation: &VerifiedBundleGeneration,
    node_trust_store: &TrustStore,
    policy: &ryeos_engine::isolation::IsolationPolicy,
    retain_captures: bool,
) -> Result<(
    Option<Arc<ryeos_engine::isolation::ResolvedIsolationBackend>>,
    Option<Arc<ryeos_engine::isolation::VerifiedIsolationBackendCapture>>,
)> {
    if policy.mode == ryeos_engine::isolation::IsolationMode::Disabled {
        return Ok((None, None));
    }
    let selection = policy.backend.clone().context(
        "enforced isolation policy requires an explicit signed bundle backend selection",
    )?;
    let record = generation
        .records()
        .iter()
        .find(|record| record.name == selection.bundle)
        .with_context(|| format!("isolation bundle `{}` is not registered", selection.bundle))?;
    let verified = ryeos_bundle::manifest::load_verified_manifest(
        &record.path.join(ryeos_engine::AI_DIR),
        &record.name,
        node_trust_store,
    )
    .context("verify selected isolation bundle manifest")?;
    let (backend, capture) = resolve_verified_isolation_backend_with_capture_retention(
        selection,
        &record.path,
        verified,
        node_trust_store,
        "",
        retain_captures,
    )?;
    Ok((Some(backend), capture))
}

/// Compose the exact isolation generation that would be selected from a
/// prospective installed-root set. Enforced policy captures and inspects the
/// candidate adapter and artifacts; disabled policy never resolves them.
/// This validates definitions, not the installer's placement as a controller.
/// It grants no process-scope capabilities; execution admission must qualify
/// those from its actual supervised process instead of reusing this snapshot.
pub fn load_prospective_isolation(
    app_root: &std::path::Path,
    bundle_roots: &[PathBuf],
    node_trust_store: &TrustStore,
    policy: &ryeos_engine::isolation::IsolationPolicy,
    policy_generation_digest: &str,
) -> Result<Arc<ryeos_engine::isolation::IsolationRuntime>> {
    let backend = resolve_isolation_backend_for_roots(
        bundle_roots,
        node_trust_store,
        policy,
        "prospective ",
    )?;
    ryeos_engine::isolation::IsolationRuntime::resolve_compiled_policy_for_definition_validation(
        app_root,
        policy.clone(),
        crate::node_policy::generation::policy_directory(app_root).join("isolation.yaml"),
        format!("sha256:{policy_generation_digest}"),
        backend,
    )
    .map(Arc::new)
    .map_err(anyhow::Error::from)
}

fn resolve_isolation_backend_for_roots(
    bundle_roots: &[PathBuf],
    node_trust_store: &TrustStore,
    policy: &ryeos_engine::isolation::IsolationPolicy,
    diagnostic_prefix: &str,
) -> Result<Option<Arc<ryeos_engine::isolation::ResolvedIsolationBackend>>> {
    if policy.mode == ryeos_engine::isolation::IsolationMode::Disabled {
        return Ok(None);
    }
    let selection = policy.backend.clone().context(
        "enforced isolation policy requires an explicit signed bundle backend selection",
    )?;
    let mut selected = None;
    for root in bundle_roots {
        let Ok(manifest) = ryeos_bundle::manifest::load_verified_manifest(
            &root.join(ryeos_engine::AI_DIR),
            &selection.bundle,
            node_trust_store,
        ) else {
            continue;
        };
        if selected.replace((root, manifest)).is_some() {
            anyhow::bail!(
                "{diagnostic_prefix}bundle set contains more than one selected isolation bundle `{}`",
                selection.bundle
            );
        }
    }
    let (root, manifest) = selected.with_context(|| {
        format!(
            "{diagnostic_prefix}bundle set removes selected isolation bundle `{}`",
            selection.bundle
        )
    })?;
    resolve_verified_isolation_backend(
        selection,
        root,
        manifest,
        node_trust_store,
        diagnostic_prefix,
    )
    .map(Some)
}

/// Compose a real execution-admission isolation generation from exact signed
/// bundle roots. This is deliberately test-support only: production nodes use
/// the registered generation and its daemon-owned policy snapshot.
#[cfg(any(test, feature = "test-support"))]
pub fn load_test_execution_isolation(
    app_root: &std::path::Path,
    bundle_roots: &[PathBuf],
    node_trust_store: &TrustStore,
    policy: ryeos_engine::isolation::IsolationPolicy,
) -> Result<Arc<ryeos_engine::isolation::IsolationRuntime>> {
    let backend = resolve_isolation_backend_for_roots(
        bundle_roots,
        node_trust_store,
        &policy,
        "test execution ",
    )?;
    ryeos_engine::isolation::IsolationRuntime::resolve_compiled_policy(
        app_root,
        policy,
        crate::node_policy::generation::policy_directory(app_root).join("isolation.yaml"),
        format!("sha256:{}", "0".repeat(64)),
        backend,
    )
    .map(Arc::new)
    .map_err(anyhow::Error::from)
}

/// Compose a real daemon execution-admission isolation generation for tests
/// that exercise callback IPC. Unlike definition-validation construction, this
/// retains the exact already-bound daemon socket inode before any launch.
#[cfg(any(test, feature = "test-support"))]
pub fn load_test_daemon_execution_isolation(
    app_root: &std::path::Path,
    daemon_socket: &std::path::Path,
    bundle_roots: &[PathBuf],
    node_trust_store: &TrustStore,
    policy: ryeos_engine::isolation::IsolationPolicy,
) -> Result<Arc<ryeos_engine::isolation::IsolationRuntime>> {
    let backend = resolve_isolation_backend_for_roots(
        bundle_roots,
        node_trust_store,
        &policy,
        "test daemon execution ",
    )?;
    ryeos_engine::isolation::IsolationRuntime::resolve_compiled_policy_for_daemon(
        app_root,
        daemon_socket,
        policy,
        crate::node_policy::generation::policy_directory(app_root).join("isolation.yaml"),
        format!("sha256:{}", "0".repeat(64)),
        backend,
        None,
        None,
    )
    .map(Arc::new)
    .map_err(anyhow::Error::from)
}

fn resolve_verified_isolation_backend(
    selection: ryeos_isolation_protocol::IsolationBackendSelection,
    root: &std::path::Path,
    verified: ryeos_bundle::manifest::VerifiedBundleManifest,
    node_trust_store: &TrustStore,
    diagnostic_prefix: &str,
) -> Result<Arc<ryeos_engine::isolation::ResolvedIsolationBackend>> {
    resolve_verified_isolation_backend_with_capture_retention(
        selection,
        root,
        verified,
        node_trust_store,
        diagnostic_prefix,
        false,
    )
    .map(|(backend, _)| backend)
}

fn resolve_verified_isolation_backend_with_capture_retention(
    selection: ryeos_isolation_protocol::IsolationBackendSelection,
    root: &std::path::Path,
    verified: ryeos_bundle::manifest::VerifiedBundleManifest,
    node_trust_store: &TrustStore,
    diagnostic_prefix: &str,
    retain_captures: bool,
) -> Result<(
    Arc<ryeos_engine::isolation::ResolvedIsolationBackend>,
    Option<Arc<ryeos_engine::isolation::VerifiedIsolationBackendCapture>>,
)> {
    let declaration = verified
        .manifest
        .isolation_backends
        .iter()
        .find(|declaration| declaration.id == selection.implementation)
        .cloned()
        .with_context(|| {
            format!(
                "{diagnostic_prefix}isolation bundle omits implementation `{}`",
                selection.implementation
            )
        })?;
    let target = host_isolation_target()?;
    if !declaration.targets.contains(&target) {
        anyhow::bail!("{diagnostic_prefix}isolation implementation omits the host target");
    }
    let adapter_ref = format!("bin:{}", declaration.adapter);
    let adapter = ryeos_engine::binary_resolver::capture_verified_bundle_binary_ref(
        &adapter_ref,
        root,
        node_trust_store,
    )
    .with_context(|| format!("capture {diagnostic_prefix}isolation adapter"))?;
    if adapter.captured().identity.signer_fingerprint != verified.signer_fingerprint {
        anyhow::bail!(
            "{diagnostic_prefix}isolation adapter signer does not match its bundle manifest signer"
        );
    }
    let adapter_digest = adapter.captured().identity.content_hash.clone();
    let mut artifact_captures = std::collections::BTreeMap::new();
    let mut artifact_handles = std::collections::BTreeMap::new();
    let mut artifact_digests = std::collections::BTreeMap::new();
    for (role, executable) in &declaration.artifacts {
        let executable_ref = format!("bin:{executable}");
        let artifact = ryeos_engine::binary_resolver::capture_verified_bundle_binary_ref(
            &executable_ref,
            root,
            node_trust_store,
        )
        .with_context(|| format!("capture {diagnostic_prefix}isolation artifact `{executable}`"))?;
        if artifact.captured().identity.signer_fingerprint != verified.signer_fingerprint {
            anyhow::bail!(
                "{diagnostic_prefix}isolation artifact `{executable}` signer does not match its bundle manifest signer"
            );
        }
        artifact_digests.insert(*role, artifact.captured().identity.content_hash.clone());
        artifact_handles.insert(*role, artifact.captured().handle.clone());
        artifact_captures.insert(*role, artifact);
    }
    let inspection_response = inspect_isolation_backend(
        &adapter.captured().handle,
        &artifact_handles,
        &artifact_digests,
        &declaration,
        target,
    )
    .with_context(|| format!("inspect {diagnostic_prefix}isolation backend"))?;
    let backend = ryeos_engine::isolation::ResolvedIsolationBackend {
        selection,
        declaration,
        bundle_manifest_digest: verified.body_digest,
        signer_fingerprint: verified.signer_fingerprint,
        adapter_digest,
        adapter_handle: adapter.captured().handle.clone(),
        artifact_handles,
        adapter_build: inspection_response.adapter_build,
        effective_capabilities: inspection_response.effective_capabilities,
        inspected_artifacts: inspection_response.artifacts,
    };
    backend
        .validate()
        .context("validate resolved isolation backend")?;
    let backend = Arc::new(backend);
    let capture = if retain_captures {
        Some(Arc::new(
            ryeos_engine::isolation::VerifiedIsolationBackendCapture::from_captures(
                Arc::clone(&backend),
                adapter,
                artifact_captures,
            )?,
        ))
    } else {
        None
    };
    Ok((backend, capture))
}

fn host_isolation_target() -> Result<ryeos_isolation_protocol::IsolationTargetTriple> {
    if cfg!(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_env = "gnu"
    )) {
        Ok(ryeos_isolation_protocol::IsolationTargetTriple::X86_64UnknownLinuxGnu)
    } else if cfg!(all(
        target_arch = "aarch64",
        target_os = "linux",
        target_env = "gnu"
    )) {
        Ok(ryeos_isolation_protocol::IsolationTargetTriple::Aarch64UnknownLinuxGnu)
    } else {
        anyhow::bail!("selected isolation implementation does not support this host platform")
    }
}

fn inspect_isolation_backend(
    adapter: &lillux::InheritedDescriptorAuthority,
    artifact_handles: &std::collections::BTreeMap<
        ryeos_isolation_protocol::IsolationArtifactRole,
        lillux::InheritedDescriptorAuthority,
    >,
    artifact_digests: &std::collections::BTreeMap<
        ryeos_isolation_protocol::IsolationArtifactRole,
        String,
    >,
    declaration: &ryeos_isolation_protocol::IsolationBackendDeclaration,
    target: ryeos_isolation_protocol::IsolationTargetTriple,
) -> Result<AdapterInspectionResponse> {
    #[cfg(not(unix))]
    {
        let _ = (
            adapter,
            artifact_handles,
            artifact_digests,
            declaration,
            target,
        );
        anyhow::bail!("isolation adapters require inherited descriptor support");
    }
    #[cfg(unix)]
    {
        let artifacts = artifact_handles
            .iter()
            .map(|(role, handle)| {
                let fd = handle.inherited_descriptor().map_err(anyhow::Error::msg)?;
                Ok((*role, fd))
            })
            .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
        let request = AdapterInspectionRequest {
            protocol: IsolationAdapterProtocolVersion::Current,
            target,
            backend_id: declaration.id.clone(),
            artifacts,
        };
        let request =
            serde_json::to_vec(&request).context("serialize isolation inspection request")?;
        if request.len() > ryeos_isolation_protocol::MAX_REQUEST_BYTES {
            anyhow::bail!(
                "isolation inspection request exceeds {} bytes",
                ryeos_isolation_protocol::MAX_REQUEST_BYTES
            );
        }
        let request_handle = lillux::sealed_memfd(c"ryeos-isolation-inspection", &request)
            .map_err(|error| anyhow::anyhow!("seal isolation inspection request: {error}"))?;
        let result = lillux::run(lillux::SubprocessRequest {
            cmd: adapter.path().to_string_lossy().into_owned(),
            argv0: None,
            args: vec![
                "inspect".to_string(),
                request_handle
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ],
            cwd: Some("/".to_string()),
            envs: Vec::new(),
            stdin_data: None,
            timeout: 5.0,
            limits: Some(lillux::SubprocessLimits {
                max_open_files: Some(64),
                max_stdout_bytes: Some(ryeos_isolation_protocol::MAX_RESPONSE_BYTES as u64),
                max_stderr_bytes: Some(64 * 1024),
                ..lillux::SubprocessLimits::default()
            }),
            inherited_fds: std::iter::once(adapter.clone())
                .chain(artifact_handles.values().cloned())
                .chain(std::iter::once(request_handle))
                .collect(),
            inherited_fd_mappings: Vec::new(),
            supervised_status: None,
        });
        if !result.success {
            anyhow::bail!(
                "isolation adapter inspection failed: {}",
                result.stderr.trim()
            );
        }
        let mut response: AdapterInspectionResponse =
            ryeos_isolation_protocol::from_json_str_strict(&result.stdout)
                .context("parse strict isolation adapter inspection response")?;
        if response.protocol != declaration.protocol {
            anyhow::bail!("isolation adapter returned a different protocol version");
        }
        response
            .validate()
            .context("validate isolation adapter inspection response")?;
        response.effective_capabilities =
            declaration.effective_capabilities(&response.effective_capabilities);
        if response.artifacts.len() != artifact_digests.len() {
            anyhow::bail!("isolation adapter inspected a different artifact set than declared");
        }
        for (role, expected_digest) in artifact_digests {
            let inspected = response
                .artifacts
                .get(role)
                .with_context(|| format!("isolation adapter omitted {role:?} inspection"))?;
            if &inspected.digest != expected_digest {
                anyhow::bail!("isolation adapter observed a different {role:?} artifact digest");
            }
        }
        Ok(response)
    }
}

/// Build the native engine from daemon configuration (Model B).
///
/// Thin wrapper around [`build_engine_for_roots`] that pulls the
/// daemon's operator config root from the resolved app root. Use this for
/// the daemon's startup engine; use `build_engine_for_roots` directly for
/// the per-request (pushed_head) engine overlay.
pub fn build_engine(
    config: &Config,
    generation: &VerifiedBundleGeneration,
    isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
    node_trust_store: &TrustStore,
    execution_policy: &crate::node_policy::sections::execution::NodeExecutionAdmissionPolicy,
) -> Result<(Engine, Arc<ryeos_engine::isolation::IsolationRuntime>)> {
    generation.checked(node_trust_store, || {
        build_engine_for_roots_with_isolation(
            config,
            &generation.roots(),
            Some(generation.registered_roots()),
            None, // no project root at startup — resolved per-request
            None, // no overlay — daemon's persistent trust store wins
            isolation,
            Some(node_trust_store),
            execution_policy,
        )
    })
}

/// Pure constructor: build an `Engine` for a specific set of roots.
///
/// This is the single shared engine builder used by both daemon startup
/// (via [`build_engine`]) and per-request `pushed_head` overlays (the
/// per-snapshot engine cache).
///
/// # Arguments
///
/// * `config` — daemon config (used for diagnostic/env settings only).
/// * `bundle_roots` — system-tier bundle roots installed on this node.
///   The remote node's own bundles — same for every request.
/// * `project_root` — optional materialised project root for the
///   request. Currently used for project trust loading; the engine itself
///   resolves project items via `ResolutionRoots` at request time, not from
///   this argument.
/// * `trust_overlay` — optional caller-pinned trust store to UNION
///   with the persistent trust store. Used by per-request
///   overlay so caller-trusted-only publishers can verify for the
///   thread without leaking into the remote's persistent trust.
///   `None` means use only the persistent trust store.
///
/// # Why a single constructor matters
///
/// Without this refactor, the per-request engine cache would have to
/// duplicate every load step. Having both call sites go through the
/// same function guarantees that:
/// - item resolution semantics are identical
/// - kind / parser / handler / protocol load ordering is identical
/// - boot validation runs the same way against both engine variants
/// - changes only have to land in one place
pub fn build_engine_for_roots(
    config: &Config,
    bundle_roots: &[PathBuf],
    project_root: Option<&std::path::Path>,
    trust_overlay: Option<&TrustStore>,
    isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
    execution_policy: &crate::node_policy::sections::execution::NodeExecutionAdmissionPolicy,
) -> Result<Engine> {
    build_engine_for_roots_with_isolation(
        config,
        bundle_roots,
        None,
        project_root,
        trust_overlay,
        isolation,
        None,
        execution_policy,
    )
    .map(|(engine, _isolation)| engine)
}

pub fn build_registered_engine_for_roots(
    config: &Config,
    bundle_roots: &[PathBuf],
    project_root: Option<&std::path::Path>,
    trust_overlay: Option<&TrustStore>,
    isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
    execution_policy: &crate::node_policy::sections::execution::NodeExecutionAdmissionPolicy,
) -> Result<Engine> {
    isolation
        .ensure_registered_generation_current()
        .context("verify retained bundle generation before engine construction")?;
    let captured_roots = isolation
        .registered_generation_roots()
        .context("registered engine requires a retained bundle generation")?;
    let captured = captured_roots
        .iter()
        .map(|root| &root.canonical_root)
        .collect::<std::collections::BTreeSet<_>>();
    let requested = bundle_roots
        .iter()
        .collect::<std::collections::BTreeSet<_>>();
    if captured.len() != captured_roots.len()
        || requested.len() != bundle_roots.len()
        || captured != requested
    {
        anyhow::bail!("standalone engine roots differ from its retained isolation generation");
    }
    let guard = Arc::clone(&isolation);
    let (engine, _) = build_engine_for_roots_with_isolation(
        config,
        bundle_roots,
        Some(captured_roots.to_vec()),
        project_root,
        trust_overlay,
        isolation,
        None,
        execution_policy,
    )?;
    guard
        .ensure_registered_generation_current()
        .context("verify retained bundle generation after engine construction")?;
    Ok(engine)
}

pub fn retain_daemon_generation(
    runtime: ryeos_engine::isolation::IsolationRuntime,
    app_root: PathBuf,
    generation: Arc<VerifiedBundleGeneration>,
    node_trust_store: TrustStore,
) -> ryeos_engine::isolation::IsolationRuntime {
    let roots = generation.registered_roots();
    runtime.retain_registered_generation(
        Arc::new(RetainedRegisteredGeneration {
            app_root,
            _read_lock: None,
            verified_generation: generation,
            node_trust_store: node_trust_store.clone(),
        }),
        node_trust_store,
        roots,
    )
}

fn build_engine_for_roots_with_isolation(
    config: &Config,
    bundle_roots: &[PathBuf],
    registered_bundle_roots: Option<Vec<ryeos_engine::item_resolution::RegisteredBundleRoot>>,
    project_root: Option<&std::path::Path>,
    trust_overlay: Option<&TrustStore>,
    isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
    pinned_node_trust_store: Option<&TrustStore>,
    execution_policy: &crate::node_policy::sections::execution::NodeExecutionAdmissionPolicy,
) -> Result<(Engine, Arc<ryeos_engine::isolation::IsolationRuntime>)> {
    // 1. Validate bundle roots exist and are readable
    if bundle_roots.is_empty() {
        anyhow::bail!(
            "no registered bundles found. Core bundle registration is \
             required. Run: ryeos init"
        );
    }
    for root in bundle_roots {
        if !root.is_dir() {
            tracing::warn!(
                path = %root.display(),
                "registered bundle root does not exist or is not a directory"
            );
        }
    }

    // 2. Bundle roots = registered bundles only (Model B).
    //    app_root is NOT a root — it contains node state, not content.
    let bundle_roots: Vec<PathBuf> = bundle_roots.to_vec();

    // 3. Consume one immutable node-trust snapshot for installed executable
    // and registry authority. Project keys and a caller-scoped overlay derive
    // a separate item-resolution view but can never expand node authority.
    //    Bundle-internal trust dirs are warning-only and never loaded.
    //    Trust store loads BEFORE kind schemas because kind
    //    schema verification requires the trust store. Both use raw
    //    filesystem scanning (no item resolution dependency), so there
    //    is no bootstrap cycle.
    let node_config_root = config.runtime_root().config();
    TrustStore::warn_ignored_bundle_trust_dirs(&bundle_roots);
    let pinned_node_trust_store =
        pinned_node_trust_store.or_else(|| isolation.registered_generation_node_trust());
    let node_trust_store = match pinned_node_trust_store {
        Some(store) => store.clone(),
        None => TrustStore::load(None, &node_config_root).map_err(|err| {
            tracing::error!(error = %err, "failed to load node trust store");
            anyhow::anyhow!("failed to load node trust store: {err}")
        })?,
    };
    tracing::info!(count = node_trust_store.len(), "loaded node trust store");
    let trust_store = match project_root {
        Some(project_root) => node_trust_store
            .with_project_keys(project_root)
            .map(std::borrow::Cow::into_owned),
        None => Ok(node_trust_store.clone()),
    };
    let trust_store = match trust_store {
        Ok(mut store) => {
            if let Some(overlay) = trust_overlay {
                // Caller-scoped overlay: pins the caller trusts but the
                // remote does not — never written to the remote's
                // persistent trust dir. The overlay lives for this
                // engine's lifetime only.
                let added = store.extend_from(overlay);
                tracing::info!(
                    overlay_added = added,
                    total = store.len(),
                    "applied per-request trust overlay"
                );
            }
            store
        }
        Err(err) => {
            tracing::error!(error = %err, "failed to load project item trust store");
            anyhow::bail!("failed to load project item trust store: {err}");
        }
    };

    // 4. Admit the exact installed root set through the same registry and
    // executable checks used by prospective install/replace admission. Keeping
    // this as one constructor prevents install from accepting a generation
    // that the next engine build or daemon restart would reject.
    let NodeBundleAdmission {
        kinds,
        parser_dispatcher,
        composers,
        effective_validators,
        protocol_registry,
        runtimes,
        launch_preparers,
        isolation,
    } = build_node_bundle_admission(&bundle_roots, &node_trust_store, isolation.clone())?;

    let engine = Engine::new(kinds, parser_dispatcher, bundle_roots)
        .with_registered_bundle_roots(registered_bundle_roots.unwrap_or_default())
        .with_node_config_root(config.app_root.clone())
        .with_isolation_generation(Arc::clone(&isolation))
        .with_trust_store(trust_store)
        .with_node_trust_store(node_trust_store)
        .with_composers(composers)
        .with_effective_validators(effective_validators)
        .with_protocols(protocol_registry)
        .with_runtimes(runtimes)
        .with_launch_preparers(launch_preparers)
        .with_host_env(execution_policy.host_env_bindings()?)
        .with_execution_resource_policy(
            execution_policy
                .resource_authority
                .as_ref()
                .map(|policy| policy.admission.clone()),
        );

    Ok((engine, isolation))
}

/// Admit a prospective node bundle-root set without constructing an Engine.
///
/// Install and replace handlers call this against the exact post-operation
/// graph before activation. Daemon boot shares the private registry constructor,
/// but separately qualifies its host execution facilities from its supervised
/// placement. Do not retain this definition-only snapshot for live execution.
pub fn admit_node_bundle_roots(
    app_root: &std::path::Path,
    bundle_roots: &[PathBuf],
    node_trust_store: &TrustStore,
    policy: &ryeos_engine::isolation::IsolationPolicy,
    policy_generation_digest: &str,
) -> Result<Arc<ryeos_engine::isolation::IsolationRuntime>> {
    let prospective = load_prospective_isolation(
        app_root,
        bundle_roots,
        node_trust_store,
        policy,
        policy_generation_digest,
    )?;
    build_node_bundle_admission(bundle_roots, node_trust_store, prospective.clone())?;
    Ok(prospective)
}

/// Verify that the signed node bundle registrations and installed manifests
/// form one valid dependency/provider graph before boot consumes their roots.
///
/// `BundleBootstrapLoader` supplies signed registration names and canonical
/// roots, while `ryeos-bundle` performs the signed-manifest and BundlePlan
/// checks. Comparing both views prevents startup from validating one registry
/// generation and constructing the engine from another.
pub fn validate_installed_bundle_plan(
    app_root: &std::path::Path,
    generation: &VerifiedBundleGeneration,
    node_trust_store: &TrustStore,
) -> Result<ryeos_bundle::plan::BundlePlan> {
    let bundle_records = generation.records();
    let installed =
        ryeos_bundle::installed::load_installed_plan_inputs_with_trust(app_root, node_trust_store)
            .context("load signed installed bundle manifests")?;
    if installed.len() != bundle_records.len() {
        anyhow::bail!(
            "installed bundle registry views disagree: node config loaded {} record(s), manifest planner loaded {}",
            bundle_records.len(),
            installed.len()
        );
    }

    for record in bundle_records {
        let planned = installed
            .iter()
            .find(|input| input.name == record.name)
            .with_context(|| {
                format!(
                    "installed bundle '{}' is absent from the signed manifest-planner view",
                    record.name
                )
            })?;
        if planned.source.root_path() != &record.path {
            anyhow::bail!(
                "installed bundle '{}' registry path mismatch: node config resolved {}, manifest planner resolved {}",
                record.name,
                record.path.display(),
                planned.source.root_path().display()
            );
        }
    }

    ryeos_bundle::plan::build_plan(
        ryeos_bundle::plan::BundlePlanMode::VerifyInstalled,
        &[],
        &installed,
    )
    .context("validate installed bundle dependency/provider graph")
}

struct NodeBundleAdmission {
    kinds: KindRegistry,
    parser_dispatcher: ParserDispatcher,
    composers: ComposerRegistry,
    effective_validators: EffectiveValidatorRegistry,
    protocol_registry: ProtocolRegistry,
    runtimes: RuntimeRegistry,
    launch_preparers: LaunchPreparerRegistry,
    isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
}

fn build_node_bundle_admission(
    bundle_roots: &[PathBuf],
    node_trust_store: &TrustStore,
    isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
) -> Result<NodeBundleAdmission> {
    if bundle_roots.is_empty() {
        anyhow::bail!("prospective node bundle set is empty");
    }

    let schema_roots: Vec<PathBuf> = bundle_roots
        .iter()
        .map(|root| {
            root.join(ryeos_engine::AI_DIR)
                .join(ryeos_engine::KIND_SCHEMAS_DIR)
        })
        .filter(|root| root.is_dir())
        .collect();

    ryeos_engine::binary_resolver::verify_bundle_executor_manifests(bundle_roots, node_trust_store)
        .context("node bundle executor admission failed")?;

    let kinds = if schema_roots.is_empty() {
        anyhow::bail!(
            "no kind schema roots found in prospective node bundle set under {}/{}",
            ryeos_engine::AI_DIR,
            ryeos_engine::KIND_SCHEMAS_DIR
        );
    } else {
        KindRegistry::load_base(&schema_roots, node_trust_store)
            .context("failed to load kind schemas")?
    };
    tracing::info!(
        count = kinds.len(),
        roots = schema_roots.len(),
        kinds = %kinds.kinds().collect::<Vec<_>>().join(", "),
        "admitted kind schemas"
    );

    let (parser_tools, parser_duplicates) =
        ParserRegistry::load_base(bundle_roots, node_trust_store, &kinds)
            .context("failed to load parser tool descriptors")?;
    tracing::info!(
        count = parser_tools.len(),
        duplicates = parser_duplicates.len(),
        "admitted parser tool descriptors"
    );

    let tagged_roots: Vec<(PathBuf, TrustClass)> = bundle_roots
        .iter()
        .map(|root| (root.clone(), TrustClass::TrustedBundle))
        .collect();
    let runtimes = RuntimeRegistry::build_from_bundles(&tagged_roots, node_trust_store, &kinds)
        .context("failed to build runtime registry")?;
    let handler_registry =
        HandlerRegistry::load_base(&tagged_roots, node_trust_store, isolation.clone())
            .context("failed to load handler descriptors from bundle roots")?;
    let handler_registry = std::sync::Arc::new(handler_registry);
    let protocol_registry = ProtocolRegistry::load_base(&tagged_roots, node_trust_store)
        .context("failed to load protocol descriptors from bundle roots")?;
    let composers = ComposerRegistry::from_kinds(&kinds, &handler_registry)
        .context("failed to derive composer registry from kind schemas")?;
    let effective_validators = EffectiveValidatorRegistry::from_kinds(
        &kinds,
        &handler_registry,
        isolation.clone(),
        bundle_roots,
    )
    .context("failed to bind effective validators from kind schemas")?;

    validate_terminator_refs(&kinds, &protocol_registry)
        .context("terminator→protocol ref validation failed")?;
    if let Err(issues) = validate_boot(
        &kinds,
        &parser_tools,
        &handler_registry,
        &composers,
        &parser_duplicates,
    ) {
        let mut message = String::from("prospective node boot validation failed:\n");
        for issue in &issues {
            message.push_str(&format!("  - {issue:?}\n"));
        }
        anyhow::bail!("{message}");
    }
    if let Err(issues) = validate_protocol_builder(&kinds, &protocol_registry) {
        let mut message = String::from("prospective protocol builder validation failed:\n");
        for issue in &issues {
            message.push_str(&format!("  - {issue:?}\n"));
        }
        anyhow::bail!("{message}");
    }

    // Enforced policy binds preparers to the backend captured while the
    // immutable isolation snapshot was loaded. Disabled policy binds the same
    // verified handlers to the direct bounded runner without resolving or
    // probing a selected isolation adapter.
    let launch_preparers = if runtimes.requires_launch_preparer() {
        bind_launch_preparers(
            &runtimes,
            &handler_registry,
            isolation.clone(),
            bundle_roots,
        )?
    } else {
        LaunchPreparerRegistry::default()
    };

    tracing::info!(
        runtimes = runtimes.all().count(),
        roots = tagged_roots.len(),
        "prospective node bundle set admitted"
    );

    Ok(NodeBundleAdmission {
        kinds,
        parser_dispatcher: ParserDispatcher::new(parser_tools, handler_registry.clone()),
        composers,
        effective_validators,
        protocol_registry,
        runtimes,
        launch_preparers,
        isolation,
    })
}

fn bind_launch_preparers(
    runtimes: &RuntimeRegistry,
    handlers: &HandlerRegistry,
    isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
    bundle_roots: &[PathBuf],
) -> Result<LaunchPreparerRegistry> {
    let runner = LaunchPreparerRunner::from_isolation_runtime(isolation, bundle_roots)
        .context("failed to initialize fixed launch-preparer isolation")?;
    if let Err(issues) = validate_runtime_launch_handlers(runtimes, handlers, &runner) {
        let mut message = String::from("runtime launch-preparer validation failed:\n");
        for issue in &issues {
            message.push_str(&format!("  - {issue:?}\n"));
        }
        anyhow::bail!("{message}");
    }
    LaunchPreparerRegistry::from_runtimes(runtimes, handlers, runner)
        .context("failed to bind runtime launch preparers")
}

/// Walk every kind schema's terminator and verify that every statically named
/// or selector-allowlisted subprocess protocol resolves in the registry.
fn validate_terminator_refs(kinds: &KindRegistry, protocols: &ProtocolRegistry) -> Result<()> {
    for kind_name in kinds.kinds() {
        if let Some(schema) = kinds.get(kind_name)
            && let Some(exec) = &schema.execution
            && let Some(TerminatorDecl::Subprocess { protocol }) = &exec.terminator
        {
            for protocol_ref in protocol.boot_required_refs() {
                protocols.require(protocol_ref).with_context(|| {
                    format!(
                        "kind `{kind_name}` declares protocol `{protocol_ref}` \
                         but no such protocol is registered in the protocol registry"
                    )
                })?;
            }
        }
    }
    Ok(())
}

/// Future root-allocated C3a fixture. Preparing signs/copies temporary inputs;
/// admitting executes the real adapter inspection. Neither is invoked here.
#[cfg(all(test, unix))]
pub(crate) mod temporary_registered_backend_fixture {
    use super::*;
    use base64::Engine as _;
    use ryeos_isolation_protocol::{
        IsolationBackendDeclaration, IsolationCapability, IsolationTargetTriple,
    };
    use std::os::unix::fs::PermissionsExt as _;

    const BUNDLE: &str = "c3a-fixture";
    const MAX_ADAPTER_BYTES: u64 = 16 * 1024 * 1024;

    pub(crate) struct PreparedRealm {
        temporary: tempfile::TempDir,
        bundle: PathBuf,
        adapter_path: PathBuf,
        publisher_key_path: PathBuf,
        publisher: lillux::crypto::SigningKey,
        declaration: IsolationBackendDeclaration,
        node_identity: crate::identity::NodeIdentity,
    }

    pub(crate) struct AdmittedRealm {
        pub(crate) runtime: Arc<ryeos_engine::isolation::IsolationRuntime>,
        // Retain actual signed registry, keys, policy, executable/CAS roots.
        pub(crate) realm: PreparedRealm,
    }

    /// Untrusted measurement inputs, not a capture or inspection response.
    /// The caller must supply freshly measured root-reviewed executable bytes.
    pub(crate) fn prepare(
        adapter_bytes: &[u8],
        expected_sha256: &str,
        expected_size: u64,
        expected_mode: u32,
        target: IsolationTargetTriple,
        declaration: IsolationBackendDeclaration,
        profile: crate::node_policy::generation::NodeInitProfile,
    ) -> Result<PreparedRealm> {
        anyhow::ensure!(
            expected_size > 0
                && expected_size <= MAX_ADAPTER_BYTES
                && adapter_bytes.len() as u64 == expected_size
                && expected_mode == 0o755
                && lillux::valid_hash(expected_sha256)
                && lillux::sha256_hex(adapter_bytes) == expected_sha256,
            "temporary C3a adapter contradicts its measured bytes/size/mode"
        );
        declaration.validate().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            target == host_isolation_target()?
                && declaration.targets == [target]
                && declaration.id == "linux-lillux"
                && declaration.artifacts.is_empty()
                && declaration.protocol == IsolationAdapterProtocolVersion::Current
                && declaration
                    .capabilities
                    .contains(&IsolationCapability::ProcessOriginalNamespaceTerminalReporting),
            "temporary C3a requires the exact current host terminal-reporter declaration"
        );
        anyhow::ensure!(
            profile.exact_bundles() == [BUNDLE],
            "temporary C3a profile must select only its actual fixture Bundle"
        );
        let table = crate::node_policy::NodePolicyTable::new();
        let generation =
            profile.validated_generation(&table, std::path::Path::new("c3a-profile.yaml"))?;
        let temporary = tempfile::tempdir()?;
        let app_root = temporary.path().canonicalize()?;
        let node_identity = crate::identity::NodeIdentity::create(
            &app_root.join(".ai/node/identity/private_key.pem"),
        )?;
        let compiled = crate::node_policy::compile_generation(
            &app_root,
            &table,
            &generation,
            node_identity.fingerprint(),
        )?;
        let policy = compiled.require::<ryeos_engine::isolation::IsolationPolicy>()?;
        anyhow::ensure!(
            policy.mode == ryeos_engine::isolation::IsolationMode::Enforce
                && policy
                    .backend
                    .as_ref()
                    .is_some_and(|selection| selection.bundle == BUNDLE
                        && selection.implementation == declaration.id),
            "temporary C3a complete policy must enforce its actual selected backend"
        );
        // Independent node trust is persisted outside the source Bundle.
        let publisher = lillux::crypto::SigningKey::from_bytes(&[117; 32]);
        let trusted = app_root.join(".ai/config/keys/trusted");
        std::fs::create_dir_all(&trusted)?;
        let publisher_key_path = trusted.join("temporary-publisher.pub");
        std::fs::write(
            &publisher_key_path,
            base64::engine::general_purpose::STANDARD.encode(publisher.verifying_key().as_bytes()),
        )?;
        std::fs::write(
            trusted.join("temporary-node.pub"),
            base64::engine::general_purpose::STANDARD
                .encode(node_identity.verifying_key().as_bytes()),
        )?;
        let trust = TrustStore::load(None, &app_root.join(".ai/config"))?;

        let bundle = app_root.join(".ai/bundles").join(BUNDLE);
        let ai = bundle.join(ryeos_engine::AI_DIR);
        let target_name = lillux::platform::current_binary_target()
            .context("temporary C3a host binary target unavailable")?;
        let bin = ai.join("bin").join(target_name);
        std::fs::create_dir_all(&bin)?;
        let adapter_path = bin.join(&declaration.adapter);
        std::fs::write(&adapter_path, adapter_bytes)?;
        std::fs::set_permissions(
            &adapter_path,
            std::fs::Permissions::from_mode(expected_mode),
        )?;
        let cas = lillux::cas::CasStore::new(ai.join("objects"));
        let blob = cas.store_blob(adapter_bytes)?;
        anyhow::ensure!(
            blob == expected_sha256,
            "temporary C3a stored adapter identity changed"
        );
        let item_ref = format!("bin/{target_name}/{}", declaration.adapter);
        let item_source = serde_json::json!({
            "kind": "item_source", "item_ref": item_ref,
            "content_blob_hash": blob, "integrity": format!("sha256:{blob}"),
            "mode": expected_mode, "signature_info": null,
        });
        let item_source_hash = cas.store_object(&item_source)?;
        std::fs::write(
            adapter_path.with_file_name(format!("{}.item_source.json", declaration.adapter)),
            lillux::signature::sign_content(
                &lillux::canonical_json(&item_source)?,
                &publisher,
                "#",
                None,
            ),
        )?;
        let mut items = serde_json::Map::new();
        items.insert(item_ref, serde_json::Value::String(item_source_hash));
        let manifest = serde_json::json!({"kind": "source_manifest", "item_source_hashes": items});
        let manifest_hash = cas.store_object(&manifest)?;
        let reference = ai.join("refs/bundles/manifest");
        std::fs::create_dir_all(
            reference
                .parent()
                .context("temporary executor reference parent absent")?,
        )?;
        std::fs::write(
            reference,
            lillux::signature::sign_content(
                &format!(
                    "{}\n{manifest_hash}\n",
                    ryeos_engine::executor_resolution::EXECUTOR_MANIFEST_REF_DOMAIN
                ),
                &publisher,
                "#",
                None,
            ),
        )?;
        write_bundle_manifest(&bundle, &declaration, &publisher)?;
        let registrations = app_root.join(".ai/node/bundles");
        std::fs::create_dir_all(&registrations)?;
        let registration =
            serde_yaml::to_string(&serde_json::json!({"kind": "node", "path": bundle}))?;
        std::fs::write(
            registrations.join(format!("{BUNDLE}.yaml")),
            lillux::signature::sign_content(&registration, node_identity.signing_key(), "#", None),
        )?;
        let lock = crate::state_lock::StateLock::acquire(&crate::state_lock::default_lock_path(
            &app_root,
        ))?;
        crate::node_policy::generation::publish_policy_update(
            &app_root,
            &crate::node_policy::generation::NodePolicyUpdate::initial(generation),
            &node_identity,
            &trust,
            &lock,
        )?;
        drop(lock);
        Ok(PreparedRealm {
            temporary,
            bundle,
            adapter_path,
            publisher_key_path,
            publisher,
            declaration,
            node_identity,
        })
    }

    fn write_bundle_manifest(
        bundle: &std::path::Path,
        declaration: &IsolationBackendDeclaration,
        publisher: &lillux::crypto::SigningKey,
    ) -> Result<()> {
        let body = serde_yaml::to_string(&serde_json::json!({
            "name": BUNDLE, "version": "1.0.0", "description": "temporary C3a only",
            "provides_kinds": [], "requires_kinds": [], "uses_kinds": [],
            "isolation_backends": [declaration],
        }))?;
        std::fs::write(
            bundle.join(".ai/manifest.yaml"),
            lillux::signature::sign_content(&body, publisher, "#", None),
        )?;
        Ok(())
    }

    impl PreparedRealm {
        pub(crate) fn app_root(&self) -> &std::path::Path {
            self.temporary.path()
        }

        /// NATIVE boundary: production locked loader performs actual adapter
        /// inspection. Future root allocation required; no metadata fallback.
        pub(crate) fn admit(self) -> Result<AdmittedRealm> {
            let runtime = load_locked_registered_isolation(self.app_root())?;
            anyhow::ensure!(
                runtime.is_enforced(),
                "temporary C3a runtime is not enforced"
            );
            let capture = runtime
                .verified_backend_capture()
                .context("temporary C3a actual executable capture absent")?;
            anyhow::ensure!(
                capture
                    .backend()
                    .effective_capabilities
                    .contains(&IsolationCapability::ProcessOriginalNamespaceTerminalReporting),
                "temporary C3a actual inspected terminal reporter absent"
            );
            anyhow::ensure!(
                runtime
                    .registered_generation_node_trust()
                    .is_some_and(|trust| trust.is_trusted(self.node_identity.fingerprint())),
                "temporary C3a independent node trust absent"
            );
            let operation = runtime
                .begin_registered_generation_operation()?
                .context("temporary C3a registered generation operation absent")?;
            runtime.ensure_registered_generation_current()?;
            anyhow::ensure!(
                runtime
                    .registered_generation_roots()
                    .is_some_and(|roots| roots.len() == 1
                        && roots[0].name == BUNDLE
                        && roots[0].canonical_root == self.bundle),
                "temporary C3a registered roots differ from retained realm"
            );
            drop(operation);
            Ok(AdmittedRealm {
                runtime,
                realm: self,
            })
        }

        /// Conditional real-loader refusal control. Caller supplies a fresh
        /// prepared realm per mutation; no successful runtime is fabricated.
        pub(crate) fn require_loader_refusal(&mut self, mutation: Refusal) -> Result<()> {
            let expected = match mutation {
                Refusal::RevokedPublisher => {
                    std::fs::remove_file(&self.publisher_key_path)?;
                    "not node-trusted"
                }
                Refusal::ChangedSignedBundleBody => {
                    let path = self.bundle.join(".ai/manifest.yaml");
                    let mut bytes = std::fs::read(&path)?;
                    bytes.extend_from_slice(b"\n# unsigned mutation\n");
                    std::fs::write(path, bytes)?;
                    "verify bundle manifest"
                }
                Refusal::ChangedAdapterBytes => {
                    let mut bytes = std::fs::read(&self.adapter_path)?;
                    let first = bytes.first_mut().context("temporary adapter empty")?;
                    *first ^= 1;
                    std::fs::write(&self.adapter_path, bytes)?;
                    "capture isolation adapter"
                }
                Refusal::ForeignHostTarget => {
                    let mut declaration = self.declaration.clone();
                    declaration.targets = vec![match host_isolation_target()? {
                        IsolationTargetTriple::X86_64UnknownLinuxGnu => {
                            IsolationTargetTriple::Aarch64UnknownLinuxGnu
                        }
                        IsolationTargetTriple::Aarch64UnknownLinuxGnu => {
                            IsolationTargetTriple::X86_64UnknownLinuxGnu
                        }
                    }];
                    write_bundle_manifest(&self.bundle, &declaration, &self.publisher)?;
                    "implementation omits the host target"
                }
            };
            match load_locked_registered_isolation(self.app_root()) {
                Ok(_) => anyhow::bail!("temporary C3a mutation unexpectedly admitted a runtime"),
                Err(error) => {
                    anyhow::ensure!(
                        format!("{error:#}").contains(expected),
                        "temporary C3a refusal did not reach its intended boundary: {error:#}"
                    );
                    Ok(())
                }
            }
        }
    }

    pub(crate) enum Refusal {
        RevokedPublisher,
        ChangedSignedBundleBody,
        ChangedAdapterBytes,
        ForeignHostTarget,
    }
}

#[cfg(test)]
mod isolation_generation_tests {
    use super::*;

    #[test]
    fn retained_generation_operation_allows_nested_generation_read_lock() {
        let app_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(app_root.path().join(ryeos_engine::AI_DIR).join("bundles"))
            .unwrap();
        // Initialize the lock anchor the same way node setup does.
        drop(crate::registry::BundleRegistryMutationLock::acquire(app_root.path()).unwrap());

        let lifeline = RetainedRegisteredGeneration {
            app_root: app_root.path().to_path_buf(),
            _read_lock: None,
            verified_generation: Arc::new(VerifiedBundleGeneration {
                records: Vec::new(),
                bundles: Vec::new(),
            }),
            node_trust_store: TrustStore::empty(),
        };
        let operation =
            ryeos_engine::isolation::IsolationGenerationLifeline::begin_operation(&lifeline)
                .unwrap();

        let path = app_root.path().to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        let nested = std::thread::spawn(move || {
            // Exercise the generation-lock invariant directly. A complete
            // nested engine requires a real signed node and bundle generation;
            // an empty temporary root is not a valid current-schema fixture.
            let result = crate::registry::BundleRegistryReadLock::acquire_for_composition(&path)
                .map(|_| ())
                .map_err(|error| format!("{error:#}"));
            tx.send(result).unwrap();
        });
        let composition = rx.recv_timeout(std::time::Duration::from_secs(1));
        drop(operation);
        drop(lifeline);
        nested.join().unwrap();

        composition.unwrap().unwrap();
    }

    #[test]
    fn verified_generation_refuses_atomic_bundle_root_replacement() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("bundle");
        let displaced = temporary.path().join("bundle-old");
        std::fs::create_dir(&root).unwrap();
        let authority = lillux::PinnedDirectory::open(&root).unwrap().unwrap();
        let record = VerifiedBundleGenerationRecord {
            name: "demo".to_string(),
            canonical_root: root.clone(),
            root_authority: Arc::new(authority),
            manifest_body_digest: "a".repeat(64),
            manifest_signer_fingerprint: "b".repeat(64),
            executor_manifest: None,
        };

        std::fs::rename(&root, displaced).unwrap();
        std::fs::create_dir(&root).unwrap();

        let error = ensure_generation_root_binding(&record).unwrap_err();
        assert!(error.to_string().contains("root identity changed"));
    }

    fn policy(
        mode: ryeos_engine::isolation::IsolationMode,
    ) -> ryeos_engine::isolation::IsolationPolicy {
        let mut policy = ryeos_engine::isolation::IsolationPolicy::disabled_for_authoring();
        policy.mode = mode;
        if mode == ryeos_engine::isolation::IsolationMode::Enforce {
            policy.backend = Some(ryeos_isolation_protocol::IsolationBackendSelection {
                bundle: "example-isolation-backend".to_string(),
                implementation: "example".to_string(),
            });
        }
        policy
    }

    #[test]
    fn disabled_prospective_generation_does_not_require_selected_bundle() {
        let app_root = tempfile::tempdir().unwrap();
        let policy = policy(ryeos_engine::isolation::IsolationMode::Disabled);

        let runtime = load_prospective_isolation(
            app_root.path(),
            &[],
            &TrustStore::empty(),
            &policy,
            "test-policy-generation",
        )
        .expect("disabled prospective generation");
        assert!(!runtime.is_enforced());
        assert_eq!(
            runtime.inspection().backend.status,
            ryeos_engine::isolation::IsolationBackendStatus::Disabled
        );
    }

    #[test]
    fn enforced_prospective_generation_refuses_selected_bundle_removal() {
        let app_root = tempfile::tempdir().unwrap();
        let policy = policy(ryeos_engine::isolation::IsolationMode::Enforce);

        let error = match load_prospective_isolation(
            app_root.path(),
            &[],
            &TrustStore::empty(),
            &policy,
            "test-policy-generation",
        ) {
            Ok(_) => panic!("enforced prospective generation accepted a missing backend"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("removes selected isolation bundle"));
    }
}
