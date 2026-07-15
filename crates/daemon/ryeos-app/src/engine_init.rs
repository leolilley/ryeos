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

/// Build the native engine from daemon configuration (Model B).
///
/// Thin wrapper around [`build_engine_for_roots`] that pulls the
/// daemon's operator config root from the resolved app root. Use this for
/// the daemon's startup engine; use `build_engine_for_roots` directly for
/// the per-request (pushed_head) engine overlay.
pub fn build_engine(
    config: &Config,
    bundle_roots: &[PathBuf],
    sandbox: Arc<ryeos_engine::sandbox::SandboxRuntime>,
) -> Result<(Engine, Arc<ryeos_engine::sandbox::SandboxRuntime>)> {
    build_engine_for_roots_with_sandbox(
        config,
        bundle_roots,
        None, // no project root at startup — resolved per-request
        None, // no overlay — daemon's persistent trust store wins
        sandbox,
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
    sandbox: Arc<ryeos_engine::sandbox::SandboxRuntime>,
) -> Result<Engine> {
    build_engine_for_roots_with_sandbox(config, bundle_roots, project_root, trust_overlay, sandbox)
        .map(|(engine, _sandbox)| engine)
}

fn build_engine_for_roots_with_sandbox(
    config: &Config,
    bundle_roots: &[PathBuf],
    project_root: Option<&std::path::Path>,
    trust_overlay: Option<&TrustStore>,
    sandbox: Arc<ryeos_engine::sandbox::SandboxRuntime>,
) -> Result<(Engine, Arc<ryeos_engine::sandbox::SandboxRuntime>)> {
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
        sandbox,
    } = build_node_bundle_admission(&bundle_roots, &node_trust_store, sandbox.clone())?;

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

    Ok((engine, sandbox))
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
    sandbox: Arc<ryeos_engine::sandbox::SandboxRuntime>,
) -> Result<()> {
    build_node_bundle_admission(bundle_roots, node_trust_store, sandbox).map(|_| ())
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
    sandbox: Arc<ryeos_engine::sandbox::SandboxRuntime>,
}

fn build_node_bundle_admission(
    bundle_roots: &[PathBuf],
    node_trust_store: &TrustStore,
    sandbox: Arc<ryeos_engine::sandbox::SandboxRuntime>,
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
        HandlerRegistry::load_base(&tagged_roots, node_trust_store, sandbox.clone())
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

    // Validate the complete preparer registry against a detached exact backend
    // capture before publishing it into the daemon snapshot. If another fully
    // validated admission wins publication concurrently, validate and bind
    // this graph again against that winner before activation can proceed.
    let (sandbox, launch_preparers) = if runtimes.requires_launch_preparer() {
        let tentative = sandbox
            .tentative_mandatory_bubblewrap_backend()
            .context("failed to tentatively capture sandbox backend required by runtimes")?;
        let tentative_preparers = bind_launch_preparers(&runtimes, &handler_registry, &tentative)?;
        let (published, reconciled) = sandbox
            .publish_mandatory_bubblewrap_backend(&tentative)
            .context("failed to publish admitted launch-preparer sandbox backend")?;
        let launch_preparers = if reconciled {
            bind_launch_preparers(&runtimes, &handler_registry, &published)?
        } else {
            tentative_preparers
        };
        (Arc::new(published), launch_preparers)
    } else {
        (sandbox, LaunchPreparerRegistry::default())
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
        sandbox,
    })
}

fn bind_launch_preparers(
    runtimes: &RuntimeRegistry,
    handlers: &HandlerRegistry,
    sandbox: &ryeos_engine::sandbox::SandboxRuntime,
) -> Result<LaunchPreparerRegistry> {
    let runner = LaunchPreparerRunner::from_sandbox_runtime(sandbox)
        .context("failed to initialize fixed launch-preparer sandbox")?;
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
