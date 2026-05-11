//! Engine initialization for ryeosd.
//!
//! Constructs a `ryeos_engine::engine::Engine` at daemon startup using
//! the daemon's config-driven system data directory and user space.
//! The engine crate is kind-agnostic — all kind definitions come from
//! `*.kind-schema.yaml` files found under `{AI_DIR}/node/engine/kinds/`.

use std::path::PathBuf;

use anyhow::{Context, Result};

use ryeos_engine::boot_validation::{validate_boot, validate_protocol_builder};
use ryeos_engine::composers::ComposerRegistry;
use ryeos_engine::engine::Engine;
use ryeos_engine::runtime::HostEnvBindings;
use ryeos_engine::handlers::HandlerRegistry;
use ryeos_engine::kind_registry::{KindRegistry, TerminatorDecl};
use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};
use ryeos_engine::protocols::ProtocolRegistry;
use ryeos_engine::resolution::TrustClass;
use ryeos_engine::roots;
use ryeos_engine::runtime_registry::RuntimeRegistry;
use ryeos_engine::trust::TrustStore;

use crate::config::Config;

/// Build the native engine from daemon configuration.
///
/// Scans the config-provided system data directory and user space for kind
/// schema files, loads the trust store from the daemon's trusted keys
/// directory. Uses the provided `bundle_roots` for item resolution.
pub fn build_engine(config: &Config, bundle_roots: &[PathBuf]) -> Result<Engine> {
    // 1. Validate bundle roots exist and are readable
    for root in bundle_roots {
        if !root.is_dir() {
            tracing::warn!(
                path = %root.display(),
                "configured bundle root does not exist or is not a directory"
            );
        }
    }

    // 2. Collect all system roots (system_space_dir + bundle_roots, ordered)
    let mut system_roots = vec![config.system_space_dir.clone()];
    system_roots.extend(bundle_roots.iter().cloned());
    let user_root = roots::user_root().ok();

    // 3. Collect kind schema search roots from all system roots + user space
    let mut schema_roots = Vec::new();

    for root in &system_roots {
        let kinds_dir = root.join(ryeos_engine::AI_DIR).join(ryeos_engine::KIND_SCHEMAS_DIR);
        if kinds_dir.is_dir() {
            schema_roots.push(kinds_dir);
        }
    }

    if let Some(ref ur) = user_root {
        let user_kinds = ur.join(ryeos_engine::AI_DIR).join(ryeos_engine::KIND_SCHEMAS_DIR);
        if user_kinds.is_dir() {
            schema_roots.push(user_kinds);
        }
    }

    // 4. Load trust store. Trust is operator-tier ONLY (project > user);
    //    system_roots is preserved in the call signature for diagnostic
    //    warnings about legacy bundle-internal trust dirs but is NOT
    //    consulted for trust admission. Trust store loads BEFORE kind
    //    schemas because kind schema verification requires the trust
    //    store. Both use raw filesystem scanning (no item resolution
    //    dependency), so there is no bootstrap cycle.
    let trust_store = match TrustStore::load_three_tier(
        None, // project root not known at daemon startup — resolved per-request
        user_root.as_deref(),
        &system_roots,
    ) {
        Ok(store) => {
            tracing::info!(count = store.len(), "loaded operator trust store");
            store
        }
        Err(err) => {
            tracing::error!(error = %err, "failed to load trust store");
            anyhow::bail!("failed to load trust store: {err}");
        }
    };

    // 5. Load kind registry from filesystem (requires trust store for verification)
    let kinds = if schema_roots.is_empty() {
        anyhow::bail!("no kind schema roots found; set system_space_dir or RYEOS_SYSTEM_SPACE_DIR to a directory containing {}/{}/", ryeos_engine::AI_DIR, ryeos_engine::KIND_SCHEMAS_DIR);
    } else {
        KindRegistry::load_base(&schema_roots, &trust_store).context("failed to load kind schemas")?
    };

    if !kinds.is_empty() {
        tracing::info!(
            count = kinds.len(),
            roots = schema_roots.len(),
            kinds = %kinds.kinds().collect::<Vec<_>>().join(", "),
            "loaded kind schemas"
        );
    }

    // 6. Load parser tool descriptors using the same search roots as
    //    the kind schemas (system roots + optional user root).
    let mut parser_search_roots: Vec<PathBuf> = system_roots.clone();
    if let Some(ref ur) = user_root {
        parser_search_roots.push(ur.clone());
    }
    let (parser_tools, parser_duplicates) =
        ParserRegistry::load_base(&parser_search_roots, &trust_store, &kinds)
            .context("failed to load parser tool descriptors")?;
    tracing::info!(
        count = parser_tools.len(),
        duplicates = parser_duplicates.len(),
        "loaded parser tool descriptors"
    );

    // 7. Load handler registry from bundle roots. Hard-error on any
    //    failure: parser+composer dispatch routes through this registry,
    //    and silently degrading to an empty registry produces confusing
    //    "parser not registered" failures downstream instead of pointing
    //    at the real load problem (no signed descriptors, bad manifest,
    //    untrusted publisher, etc.).
    // Build a tier-tagged root list. System roots are TrustedSystem;
    // the optional user root is TrustedUser. Registries that record a
    // per-item trust class derive it from the originating root tier.
    let tagged_roots: Vec<(PathBuf, TrustClass)> = system_roots
        .iter()
        .map(|r| (r.clone(), TrustClass::TrustedSystem))
        .chain(
            user_root
                .iter()
                .map(|r| (r.clone(), TrustClass::TrustedUser)),
        )
        .collect();

    let handler_registry = HandlerRegistry::load_base(
        &tagged_roots,
        &trust_store,
    )
    .context("failed to load handler descriptors from bundle roots")?;
    tracing::info!(
        count = handler_registry.iter().count(),
        "loaded handler descriptors"
    );
    let handler_registry = std::sync::Arc::new(handler_registry);

    // 7b. Load protocol registry from bundle roots. Hard-error on any
    //     failure: dispatch routes through protocol descriptors for
    //     subprocess wire contracts, and silently degrading to an empty
    //     registry produces confusing errors downstream.
    let protocol_registry = ProtocolRegistry::load_base(
        &tagged_roots,
        &trust_store,
    )
    .context("failed to load protocol descriptors from bundle roots")?;
    tracing::info!(
        count = protocol_registry.iter().count(),
        "loaded protocol descriptors"
    );

    // 8. Derive the per-kind composer registry data-drivenly from
    //    the loaded kind schemas (each schema declares its
    //    `composer:` handler ref; the engine never names a kind in
    //    Rust). Composer dispatch resolves through the same
    //    `HandlerRegistry` that parser dispatch uses — composers and
    //    parsers are both subprocess handlers.
    let composers = ComposerRegistry::from_kinds(&kinds, &handler_registry)
        .context("failed to derive composer registry from kind schemas")?;

    // 9b. Validate that every Subprocess terminator's protocol_ref
    //     resolves in the protocol registry. Unresolved refs are a
    //     hard boot error — the daemon cannot dispatch a kind whose
    //     protocol is unknown.
    validate_terminator_refs(&kinds, &protocol_registry)
        .context("terminator→protocol ref validation failed")?;

    // 9c. Cross-registry boot validation: every parser ref a kind extension
    //    cites must resolve to a known descriptor + handler + valid config,
    //    every kind's declared composer handler ref must resolve and its
    //    composer_config must pass the handler's subprocess validation,
    //    and every registered composer must point at a known kind. Collect
    //    ALL issues, then bail with a single block listing them.
    if let Err(issues) = validate_boot(
        &kinds,
        &parser_tools,
        &handler_registry,
        &composers,
        &parser_duplicates,
    ) {
        let mut msg = String::from("boot validation failed:\n");
        for issue in &issues {
            msg.push_str(&format!("  - {issue:?}\n"));
        }
        anyhow::bail!("{msg}");
    }

    // 9d. Protocol builder validation: exercise every protocol descriptor
    //    with synthetic inputs to catch descriptor regressions (unknown
    //    env sources, duplicate keys, stdin-serialize failures) at boot
    //    time. Also verifies runtime/streaming_tool kind↔protocol coupling.
    if let Err(issues) = validate_protocol_builder(&kinds, &protocol_registry) {
        let mut msg = String::from("protocol builder validation failed:\n");
        for issue in &issues {
            msg.push_str(&format!("  - {issue:?}\n"));
        }
        anyhow::bail!("{msg}");
    }

    // 10. Build parser dispatcher and construct the engine, persisting
    //     the SAME `composers` instance used by boot validation. The
    //     launcher reads the registry back off the engine — there is
    //     no second construction site that could drift.
    let parser_dispatcher = ParserDispatcher::new(parser_tools, handler_registry);

    // Scan every bundle root (system + user, when present) for verified
    // `kind: runtime` YAMLs. Fail-closed on any verification error or
    // multi-default conflict — runtime catalog drift is a startup-time
    // problem, not a per-request one.
    let runtimes = RuntimeRegistry::build_from_bundles(&tagged_roots, &trust_store, &kinds)
        .context("failed to build runtime registry")?;
    tracing::info!(
        count = runtimes.all().count(),
        roots = tagged_roots.len(),
        "loaded runtime registry"
    );

    let engine = Engine::new(kinds, parser_dispatcher, user_root, system_roots)
        .with_trust_store(trust_store)
        .with_composers(composers)
        .with_protocols(protocol_registry)
        .with_runtimes(runtimes)
        .with_host_env(load_host_env_passthrough_allowlist()?);

    Ok(engine)
}

/// Parse `RYEOS_TOOL_ENV_PASSTHROUGH` (a comma-separated list of
/// allowed host-env var names) once at startup and build a
/// `HostEnvBindings`. Empty or unset is fine — the common case
/// produces an empty allowlist.
fn load_host_env_passthrough_allowlist() -> Result<HostEnvBindings> {
    let raw = std::env::var("RYEOS_TOOL_ENV_PASSTHROUGH").unwrap_or_default();
    let names: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    let bindings = HostEnvBindings::from_allowlist(names).map_err(|e| {
        anyhow::anyhow!("invalid RYEOS_TOOL_ENV_PASSTHROUGH configuration: {e}")
    })?;
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
fn validate_terminator_refs(
    kinds: &KindRegistry,
    protocols: &ProtocolRegistry,
) -> Result<()> {
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

