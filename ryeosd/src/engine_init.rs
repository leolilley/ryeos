//! Engine initialization for ryeosd.
//!
//! Constructs a `ryeos_engine::engine::Engine` at daemon startup using
//! the daemon's config-driven system data directory and user space.
//! The engine crate is kind-agnostic — all kind definitions come from
//! `*.kind-schema.yaml` files found under `{AI_DIR}/config/engine/kinds/`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::BaseDirs;

use ryeos_engine::engine::Engine;
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::metadata::MetadataParserRegistry;
use ryeos_engine::trust::TrustStore;

use crate::config::Config;

/// Build the native engine from daemon configuration.
///
/// Scans the config-provided system data directory and user space for kind
/// schema files, loads the trust store from the daemon's trusted keys
/// directory.
pub fn build_engine(config: &Config) -> Result<Engine> {
    // 1. Validate bundle roots exist and are readable
    for root in &config.bundle_roots {
        if !root.is_dir() {
            tracing::warn!(
                path = %root.display(),
                "configured bundle root does not exist or is not a directory"
            );
        }
    }

    // 2. Collect all system roots (system_data_dir + bundle_roots, ordered)
    let system_roots = config.all_system_roots();
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

    // 4. Load trust store with three-tier resolution (project > user > system)
    //    Trust store loads BEFORE kind schemas because kind schema verification
    //    requires the trust store. Both use raw filesystem scanning (no item
    //    resolution dependency), so there is no bootstrap cycle.
    let trust_store = match TrustStore::load_three_tier(
        None, // project root not known at daemon startup — resolved per-request
        user_root.as_deref(),
        &system_roots,
    ) {
        Ok(store) => {
            tracing::info!(count = store.len(), "loaded trust store (three-tier)");
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

    // 6. Build metadata parser registry with builtins
    let parsers = MetadataParserRegistry::with_builtins();

    // 7. Construct engine
    let engine = Engine::new(kinds, parsers, user_root, system_roots)
        .with_trust_store(trust_store);

    Ok(engine)
}

/// Discover the user-space root (parent of `~/.ai/`).
fn discover_user_root() -> Option<PathBuf> {
    std::env::var_os("USER_SPACE")
        .map(PathBuf::from)
        .or_else(|| BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
}
