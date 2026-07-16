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
use ryeos_engine::engine::Engine;
use ryeos_engine::handlers::HandlerRegistry;
use ryeos_engine::kind_registry::{KindRegistry, TerminatorDecl};
use ryeos_engine::launch_preparers::{LaunchPreparerRegistry, LaunchPreparerRunner};
use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};
use ryeos_engine::protocols::ProtocolRegistry;
use ryeos_engine::resolution::TrustClass;
use ryeos_engine::runtime::HostEnvBindings;
use ryeos_engine::runtime_registry::RuntimeRegistry;
use ryeos_engine::trust::TrustStore;

use crate::config::Config;
use crate::node_config::BundleRecord;
use ryeos_isolation_protocol::{
    AdapterInspectionRequest, AdapterInspectionResponse, IsolationAdapterProtocolVersion,
};

pub fn load_registered_isolation(
    app_root: &std::path::Path,
) -> Result<Arc<ryeos_engine::isolation::IsolationRuntime>> {
    let trust_store = TrustStore::load(None, &app_root.join(ryeos_engine::AI_DIR).join("config"))
        .context("load node trust for isolation backend resolution")?;
    let loader = crate::node_config::loader::BootstrapLoader {
        app_root,
        trust_store: &trust_store,
    };
    let records = loader
        .load_bundle_section()
        .context("load bundle registrations for isolation backend resolution")?;
    let backend = resolve_isolation_backend(app_root, &records, &trust_store)?;
    ryeos_engine::isolation::IsolationRuntime::load_with_backend(app_root, backend)
        .map(Arc::new)
        .map_err(anyhow::Error::from)
}

pub fn resolve_isolation_backend(
    app_root: &std::path::Path,
    bundle_records: &[BundleRecord],
    node_trust_store: &TrustStore,
) -> Result<Option<Arc<ryeos_engine::isolation::ResolvedIsolationBackend>>> {
    let policy_path = app_root
        .join(ryeos_engine::AI_DIR)
        .join("node/isolation.yaml");
    let raw = std::fs::read_to_string(&policy_path)
        .with_context(|| format!("read node isolation policy {}", policy_path.display()))?;
    let policy: ryeos_engine::isolation::IsolationPolicy = serde_yaml::from_str(&raw)
        .with_context(|| format!("parse node isolation policy {}", policy_path.display()))?;
    if policy.mode == ryeos_engine::isolation::IsolationMode::Disabled {
        return Ok(None);
    }
    let record = bundle_records
        .iter()
        .find(|record| record.name == policy.backend.bundle)
        .with_context(|| {
            format!(
                "isolation bundle `{}` is not registered",
                policy.backend.bundle
            )
        })?;
    let verified = ryeos_bundle::manifest::load_verified_manifest(
        &record.path.join(ryeos_engine::AI_DIR),
        &record.name,
        node_trust_store,
    )
    .context("verify selected isolation bundle manifest")?;
    let declaration = verified
        .manifest
        .isolation_backends
        .iter()
        .find(|declaration| declaration.id == policy.backend.implementation)
        .cloned()
        .with_context(|| {
            format!(
                "bundle `{}` does not declare isolation implementation `{}`",
                record.name, policy.backend.implementation
            )
        })?;
    let target = host_isolation_target()?;
    if !declaration.targets.contains(&target) {
        anyhow::bail!("selected isolation implementation does not declare the host target");
    }
    let adapter = ryeos_engine::binary_resolver::capture_bundle_executable(
        &declaration.adapter,
        &record.path,
        node_trust_store,
    )
    .context("capture isolation adapter")?;
    if adapter.identity.signer_fingerprint != verified.signer_fingerprint {
        anyhow::bail!("isolation adapter signer does not match its bundle manifest signer");
    }
    let mut artifact_handles = std::collections::BTreeMap::new();
    let mut artifact_digests = std::collections::BTreeMap::new();
    for (role, executable) in &declaration.artifacts {
        let artifact = ryeos_engine::binary_resolver::capture_bundle_executable(
            executable,
            &record.path,
            node_trust_store,
        )
        .with_context(|| format!("capture isolation artifact `{executable}`"))?;
        if artifact.identity.signer_fingerprint != verified.signer_fingerprint {
            anyhow::bail!("isolation artifact `{executable}` signer does not match its bundle manifest signer");
        }
        artifact_digests.insert(*role, artifact.identity.content_hash);
        artifact_handles.insert(*role, artifact.handle);
    }
    let inspection_response = inspect_isolation_backend(
        &adapter.handle,
        &artifact_handles,
        &artifact_digests,
        &declaration,
        target,
    )?;
    let backend = ryeos_engine::isolation::ResolvedIsolationBackend {
        selection: policy.backend,
        declaration,
        bundle_manifest_digest: verified.body_digest,
        signer_fingerprint: verified.signer_fingerprint,
        adapter_handle: adapter.handle,
        artifact_handles,
        adapter_build: inspection_response.adapter_build,
        effective_capabilities: inspection_response.effective_capabilities,
        inspected_artifacts: inspection_response.artifacts,
    };
    backend
        .validate()
        .context("validate resolved isolation backend")?;
    Ok(Some(Arc::new(backend)))
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
    adapter: &Arc<std::fs::File>,
    artifact_handles: &std::collections::BTreeMap<
        ryeos_isolation_protocol::IsolationArtifactRole,
        Arc<std::fs::File>,
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
        use std::os::fd::AsRawFd as _;
        let artifacts = artifact_handles
            .iter()
            .map(|(role, handle)| {
                let fd = u32::try_from(handle.as_raw_fd())
                    .map_err(|_| anyhow::anyhow!("captured descriptor is negative"))?;
                Ok((*role, fd))
            })
            .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
        let request = AdapterInspectionRequest {
            protocol: IsolationAdapterProtocolVersion::V1,
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
            cmd: format!("/proc/self/fd/{}", adapter.as_raw_fd()),
            args: vec![
                "inspect".to_string(),
                request_handle.as_raw_fd().to_string(),
            ],
            cwd: Some("/".to_string()),
            envs: Vec::new(),
            stdin_data: None,
            timeout: 5.0,
            limits: Some(lillux::SubprocessLimits {
                max_open_files: Some(64),
                max_stdout_bytes: Some(ryeos_isolation_protocol::MAX_RESPONSE_BYTES as u64),
                max_stderr_bytes: Some(64 * 1024),
            }),
            inherited_fds: std::iter::once(adapter.clone())
                .chain(artifact_handles.values().cloned())
                .chain(std::iter::once(request_handle))
                .collect(),
            supervised_status: None,
        });
        if !result.success {
            anyhow::bail!(
                "isolation adapter inspection failed: {}",
                result.stderr.trim()
            );
        }
        let response: AdapterInspectionResponse =
            ryeos_isolation_protocol::from_json_str_strict(&result.stdout)
                .context("parse strict isolation adapter inspection response")?;
        if response.protocol != IsolationAdapterProtocolVersion::V1 {
            anyhow::bail!("isolation adapter returned a different protocol version");
        }
        response
            .validate()
            .context("validate isolation adapter inspection response")?;
        if !declaration
            .capabilities
            .is_subset(&response.effective_capabilities)
        {
            anyhow::bail!("isolation adapter does not provide every declared capability");
        }
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
    bundle_roots: &[PathBuf],
    isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
) -> Result<(Engine, Arc<ryeos_engine::isolation::IsolationRuntime>)> {
    build_engine_for_roots_with_isolation(
        config,
        bundle_roots,
        None, // no project root at startup — resolved per-request
        None, // no overlay — daemon's persistent trust store wins
        isolation,
    )
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
) -> Result<Engine> {
    build_engine_for_roots_with_isolation(
        config,
        bundle_roots,
        project_root,
        trust_overlay,
        isolation,
    )
    .map(|(engine, _isolation)| engine)
}

fn build_engine_for_roots_with_isolation(
    config: &Config,
    bundle_roots: &[PathBuf],
    project_root: Option<&std::path::Path>,
    trust_overlay: Option<&TrustStore>,
    isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
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

    // 3. Load two deliberately separate trust snapshots. Installed bundle
    // executable/bootstrap authority is node-only. Project keys and a
    // caller-scoped overlay may authorize project items, but can never expand
    // the node's executable or registry authority.
    //    Bundle-internal trust dirs are warning-only and never loaded.
    //    Trust store loads BEFORE kind schemas because kind
    //    schema verification requires the trust store. Both use raw
    //    filesystem scanning (no item resolution dependency), so there
    //    is no bootstrap cycle.
    let node_config_root = config.runtime_root().config();
    TrustStore::warn_ignored_bundle_trust_dirs(&bundle_roots);
    let node_trust_store = TrustStore::load(None, &node_config_root).map_err(|err| {
        tracing::error!(error = %err, "failed to load node trust store");
        anyhow::anyhow!("failed to load node trust store: {err}")
    })?;
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
        protocol_registry,
        runtimes,
        launch_preparers,
        isolation,
    } = build_node_bundle_admission(&bundle_roots, &node_trust_store, isolation.clone())?;

    let engine = Engine::new(kinds, parser_dispatcher, bundle_roots)
        .with_trust_store(trust_store)
        .with_node_trust_store(node_trust_store)
        .with_composers(composers)
        .with_protocols(protocol_registry)
        .with_runtimes(runtimes)
        .with_launch_preparers(launch_preparers)
        .with_host_env(load_host_env_passthrough_allowlist(
            &config.tool_env_passthrough,
        )?);

    Ok((engine, isolation))
}

/// Admit a prospective node bundle-root set without constructing an Engine.
///
/// Install and replace handlers call this against the exact post-operation
/// graph before activation. Daemon boot calls the same private constructor and
/// consumes the admitted registries, so the two admission surfaces cannot
/// silently drift.
pub fn admit_node_bundle_roots(
    bundle_roots: &[PathBuf],
    node_trust_store: &TrustStore,
    isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
) -> Result<()> {
    if isolation.is_enforced() {
        let selection = &isolation.inspection().backend.selection;
        let selected = bundle_roots
            .iter()
            .find_map(|root| {
                ryeos_bundle::manifest::load_verified_manifest(
                    &root.join(ryeos_engine::AI_DIR),
                    &selection.bundle,
                    node_trust_store,
                )
                .ok()
                .map(|manifest| (root, manifest))
            })
            .with_context(|| {
                format!(
                    "prospective bundle set removes selected isolation bundle `{}`",
                    selection.bundle
                )
            })?;
        let (root, manifest) = selected;
        let declaration = manifest
            .manifest
            .isolation_backends
            .iter()
            .find(|declaration| declaration.id == selection.implementation)
            .with_context(|| {
                format!(
                    "prospective isolation bundle omits implementation `{}`",
                    selection.implementation
                )
            })?;
        let adapter = ryeos_engine::binary_resolver::capture_bundle_executable(
            &declaration.adapter,
            root,
            node_trust_store,
        )
        .context("capture prospective isolation adapter")?;
        if adapter.identity.signer_fingerprint != manifest.signer_fingerprint {
            anyhow::bail!(
                "prospective isolation adapter signer does not match its bundle manifest signer"
            );
        }
        let mut artifact_handles = std::collections::BTreeMap::new();
        let mut artifact_digests = std::collections::BTreeMap::new();
        for (role, executable) in &declaration.artifacts {
            let captured = ryeos_engine::binary_resolver::capture_bundle_executable(
                executable,
                root,
                node_trust_store,
            )
            .with_context(|| format!("capture prospective isolation artifact `{executable}`"))?;
            if captured.identity.signer_fingerprint != manifest.signer_fingerprint {
                anyhow::bail!(
                    "prospective isolation artifact `{executable}` signer does not match its bundle manifest signer"
                );
            }
            artifact_digests.insert(*role, captured.identity.content_hash);
            artifact_handles.insert(*role, captured.handle);
        }
        let target = host_isolation_target()?;
        if !declaration.targets.contains(&target) {
            anyhow::bail!("prospective isolation implementation omits the host target");
        }
        inspect_isolation_backend(
            &adapter.handle,
            &artifact_handles,
            &artifact_digests,
            declaration,
            target,
        )
        .context("inspect prospective isolation backend")?;
    }
    build_node_bundle_admission(bundle_roots, node_trust_store, isolation).map(|_| ())
}

/// Verify that the signed node bundle registrations and installed manifests
/// form one valid dependency/provider graph before boot consumes their roots.
///
/// `BootstrapLoader` retains node-only registration fields such as command
/// grants, while `ryeos-bundle` performs the signed-manifest and BundlePlan
/// checks. Comparing both views prevents startup from validating one registry
/// generation and constructing the engine from another.
pub fn validate_installed_bundle_plan(
    app_root: &std::path::Path,
    bundle_records: &[BundleRecord],
) -> Result<ryeos_bundle::plan::BundlePlan> {
    let installed = ryeos_bundle::installed::load_installed_plan_inputs(app_root)
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

/// Build `HostEnvBindings` from the resolved daemon config's
/// `tool_env_passthrough` list. The `Config::load` step already
/// handled the `RYEOS_TOOL_ENV_PASSTHROUGH` env-var override, so
/// this function just receives the final merged list.
fn load_host_env_passthrough_allowlist(names: &[String]) -> Result<HostEnvBindings> {
    let bindings = HostEnvBindings::from_allowlist(names.iter().cloned())
        .map_err(|e| anyhow::anyhow!("invalid tool_env_passthrough configuration: {e}"))?;
    let allowed_names: Vec<&str> = bindings.allowed.iter().map(String::as_str).collect();
    tracing::info!(
        count = bindings.allowed.len(),
        names = ?allowed_names,
        "host env passthrough allowlist loaded"
    );
    Ok(bindings)
}

/// Walk every kind schema's terminator and verify that `Subprocess`
/// terminators' `protocol_ref` values resolve in the protocol registry.
fn validate_terminator_refs(kinds: &KindRegistry, protocols: &ProtocolRegistry) -> Result<()> {
    for kind_name in kinds.kinds() {
        if let Some(schema) = kinds.get(kind_name) {
            if let Some(exec) = &schema.execution {
                if let Some(TerminatorDecl::Subprocess { protocol_ref }) = &exec.terminator {
                    protocols.require(protocol_ref).with_context(|| {
                        format!(
                            "kind `{kind_name}` declares protocol `{protocol_ref}` \
                             but no such protocol is registered in the protocol registry"
                        )
                    })?;
                }
            }
        }
    }
    Ok(())
}
