//! Engine initialization for ryeosd.
//!
//! Constructs a `ryeos_engine::engine::Engine` at daemon startup using
//! the daemon's config-driven system data directory and user space.
//! The engine crate is kind-agnostic — all kind definitions come from
//! `*.kind-schema.yaml` files found under `{AI_DIR}/node/engine/kinds/`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::BaseDirs;

use ryeos_engine::boot_validation::validate_boot;
use ryeos_engine::composers::{ComposerRegistry, NativeComposerHandlerRegistry};
use ryeos_engine::engine::Engine;
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::parsers::{NativeParserHandlerRegistry, ParserDispatcher, ParserRegistry};
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

    // 2. Collect all system roots (system_data_dir + bundle_roots, ordered)
    let mut system_roots = vec![config.system_data_dir.clone()];
    system_roots.extend(bundle_roots.iter().cloned());
    let user_root = discover_user_root();

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
        anyhow::bail!("no kind schema roots found; set system_data_dir or RYE_SYSTEM_SPACE to a directory containing {}/{}/", ryeos_engine::AI_DIR, ryeos_engine::KIND_SCHEMAS_DIR);
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

    // 7. Build native parser handler registry
    let native_handlers = NativeParserHandlerRegistry::with_builtins();

    // 8. Build native composer handler registry, then derive the
    //    per-kind composer registry data-drivenly from the loaded
    //    kind schemas (each schema declares its `composer:` handler
    //    ID; the engine never names a kind in Rust).
    let native_composers = NativeComposerHandlerRegistry::with_builtins();
    let composers = ComposerRegistry::from_kinds(&kinds, &native_composers)
        .context("failed to derive composer registry from kind schemas")?;

    // 9. Cross-registry boot validation: every parser ref a kind extension
    //    cites must resolve to a known descriptor + handler + valid config,
    //    every kind's declared composer handler ID must resolve, and every
    //    registered composer must point at a known kind. Collect ALL
    //    issues, then bail with a single block listing them.
    if let Err(issues) = validate_boot(
        &kinds,
        &parser_tools,
        &native_handlers,
        &native_composers,
        &composers,
        &parser_duplicates,
    ) {
        let mut msg = String::from("boot validation failed:\n");
        for issue in &issues {
            msg.push_str(&format!("  - {issue:?}\n"));
        }
        anyhow::bail!("{msg}");
    }

    // 10. Build parser dispatcher and construct the engine, persisting
    //     the SAME `composers` instance used by boot validation. The
    //     launcher reads the registry back off the engine — there is
    //     no second construction site that could drift.
    let parser_dispatcher = ParserDispatcher::new(parser_tools, native_handlers);

    // Scan every bundle root (system + user, when present) for verified
    // `kind: runtime` YAMLs. Fail-closed on any verification error or
    // multi-default conflict — runtime catalog drift is a startup-time
    // problem, not a per-request one.
    let mut runtime_scan_roots: Vec<PathBuf> = system_roots.clone();
    if let Some(ref ur) = user_root {
        runtime_scan_roots.push(ur.clone());
    }
    let runtimes = RuntimeRegistry::build_from_bundles(&runtime_scan_roots, &trust_store)
        .context("failed to build runtime registry")?;
    tracing::info!(
        count = runtimes.all().count(),
        roots = runtime_scan_roots.len(),
        "loaded runtime registry"
    );

    let engine = Engine::new(kinds, parser_dispatcher, user_root, system_roots)
        .with_trust_store(trust_store)
        .with_composers(composers)
        .with_runtimes(runtimes);

    Ok(engine)
}

/// Discover the user-space root (parent of `~/.ai/`).
fn discover_user_root() -> Option<PathBuf> {
    std::env::var_os("USER_SPACE")
        .map(PathBuf::from)
        .or_else(|| BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
}
