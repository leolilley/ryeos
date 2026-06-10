//! Engine initialization for ryeosd (Model B).
//!
//! Constructs a `ryeos_engine::engine::Engine` at daemon startup using
//! only registered bundle roots — NOT the app_root itself.
//! Model B: bundles live under `<app_root>/.ai/bundles/{name}/`,
//! each registered at `<app_root>/.ai/node/bundles/{name}.yaml`.
//! The engine crate is kind-agnostic — all kind definitions come from
//! `*.kind-schema.yaml` files found under `{AI_DIR}/node/engine/kinds/`.

use std::path::PathBuf;

use anyhow::{Context, Result};

use ryeos_engine::boot_validation::{validate_boot, validate_protocol_builder};
use ryeos_engine::composers::ComposerRegistry;
use ryeos_engine::engine::Engine;
use ryeos_engine::handlers::HandlerRegistry;
use ryeos_engine::kind_registry::{KindRegistry, TerminatorDecl};
use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};
use ryeos_engine::protocols::ProtocolRegistry;
use ryeos_engine::resolution::TrustClass;
use ryeos_engine::runtime::HostEnvBindings;
use ryeos_engine::runtime_registry::RuntimeRegistry;
use ryeos_engine::trust::TrustStore;

use crate::config::Config;

/// Build the native engine from daemon configuration (Model B).
///
/// Thin wrapper around [`build_engine_for_roots`] that pulls the
/// daemon's operator config root from the resolved app root. Use this for
/// the daemon's startup engine; use `build_engine_for_roots` directly for
/// the per-request (pushed_head) engine overlay.
pub fn build_engine(config: &Config, bundle_roots: &[PathBuf]) -> Result<Engine> {
    build_engine_for_roots(
        config,
        bundle_roots,
        None, // no project root at startup — resolved per-request
        None, // no overlay — daemon's persistent trust store wins
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
) -> Result<Engine> {
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

    // 3. Collect kind schema search roots from all bundle roots.
    let mut schema_roots = Vec::new();

    for root in &bundle_roots {
        let kinds_dir = root
            .join(ryeos_engine::AI_DIR)
            .join(ryeos_engine::KIND_SCHEMAS_DIR);
        if kinds_dir.is_dir() {
            schema_roots.push(kinds_dir);
        }
    }

    // 4. Load trust store. Trust comes from project + operator config only.
    //    Bundle-internal trust dirs are warning-only and never loaded.
    //    Trust store loads BEFORE kind schemas because kind
    //    schema verification requires the trust store. Both use raw
    //    filesystem scanning (no item resolution dependency), so there
    //    is no bootstrap cycle.
    let operator_config_root = config.runtime_root().config();
    TrustStore::warn_ignored_bundle_trust_dirs(&bundle_roots);
    let trust_store = match TrustStore::load(project_root, &operator_config_root) {
        Ok(mut store) => {
            tracing::info!(count = store.len(), "loaded operator trust store");
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
            tracing::error!(error = %err, "failed to load trust store");
            anyhow::bail!("failed to load trust store: {err}");
        }
    };

    // 5. Load kind registry from filesystem (requires trust store for verification)
    let kinds = if schema_roots.is_empty() {
        anyhow::bail!("no kind schema roots found; set app_root or RYEOS_APP_ROOT to a directory containing {}/{}/", ryeos_engine::AI_DIR, ryeos_engine::KIND_SCHEMAS_DIR);
    } else {
        KindRegistry::load_base(&schema_roots, &trust_store)
            .context("failed to load kind schemas")?
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
    //    the kind schemas.
    let parser_search_roots: Vec<PathBuf> = bundle_roots.clone();
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
    // Build a tier-tagged root list. Bundle roots are TrustedBundle.
    // Registries that record a
    // per-item trust class derive it from the originating root tier.
    let tagged_roots: Vec<(PathBuf, TrustClass)> = bundle_roots
        .iter()
        .map(|r| (r.clone(), TrustClass::TrustedBundle))
        .collect();

    let handler_registry = HandlerRegistry::load_base(&tagged_roots, &trust_store)
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
    let protocol_registry = ProtocolRegistry::load_base(&tagged_roots, &trust_store)
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

    // Scan every bundle root for verified
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

    let engine = Engine::new(kinds, parser_dispatcher, bundle_roots)
        .with_trust_store(trust_store)
        .with_composers(composers)
        .with_protocols(protocol_registry)
        .with_runtimes(runtimes)
        .with_host_env(load_host_env_passthrough_allowlist(
            &config.tool_env_passthrough,
        )?);

    Ok(engine)
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
