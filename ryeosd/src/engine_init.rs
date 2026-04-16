//! Engine initialization for ryeosd.
//!
//! Constructs a `rye_engine::engine::Engine` at daemon startup using
//! the daemon's config-driven system data directory and user space.
//! The engine crate is kind-agnostic — all kind definitions come from
//! `*.kind-schema.yaml` files found under `.ai/config/engine/kinds/`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::BaseDirs;

use rye_engine::engine::Engine;
use rye_engine::executor_registry::{ExecutorRegistry, SubprocessDispatch};
use rye_engine::kind_registry::KindRegistry;
use rye_engine::metadata::MetadataParserRegistry;
use rye_engine::trust::TrustStore;

use crate::config::Config;

/// Build the native engine from daemon configuration.
///
/// Scans the config-provided system data directory and user space for kind
/// schema files, loads the trust store from the daemon's trusted keys
/// directory, and registers the terminal executor entries.
pub fn build_engine(config: &Config) -> Result<Engine> {
    // 1. Collect kind schema search roots from system + user space
    let user_root = discover_user_root();

    let mut schema_roots = Vec::new();

    let system_kinds = config.system_data_dir.join(".ai/config/engine/kinds");
    if system_kinds.is_dir() {
        schema_roots.push(system_kinds);
    }

    if let Some(ref ur) = user_root {
        let user_kinds = ur.join(".ai/config/engine/kinds");
        if user_kinds.is_dir() {
            schema_roots.push(user_kinds);
        }
    }

    // 2. Load kind registry from filesystem
    let kinds = if schema_roots.is_empty() {
        anyhow::bail!("no kind schema roots found; set system_data_dir or RYE_SYSTEM_SPACE to a directory containing .ai/config/engine/kinds/");
    } else {
        KindRegistry::load_base(&schema_roots).context("failed to load kind schemas")?
    };

    if !kinds.is_empty() {
        tracing::info!(
            count = kinds.len(),
            roots = schema_roots.len(),
            kinds = %kinds.kinds().collect::<Vec<_>>().join(", "),
            "loaded kind schemas"
        );
    }

    // 3. Build executor registry with terminal entries
    let executors = build_executor_registry();

    // 4. Build metadata parser registry with builtins
    let parsers = MetadataParserRegistry::with_builtins();

    // 5. System roots for three-tier resolution
    let system_roots = vec![config.system_data_dir.clone()];

    // 6. Load trust store with three-tier resolution (project > user > system)
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
            TrustStore::empty()
        }
    };

    // 7. Construct engine
    let engine = Engine::new(kinds, executors, parsers, user_root, system_roots)
        .with_trust_store(trust_store);

    Ok(engine)
}

/// Build the executor registry with terminal subprocess dispatchers.
///
/// `@primitive_chain` is the terminal executor that tools resolve to.
/// It uses the script's own shebang/extension to determine the interpreter.
fn build_executor_registry() -> ExecutorRegistry {
    let mut reg = ExecutorRegistry::new();

    // Terminal subprocess dispatch — no interpreter override, uses shebang
    reg.register("@primitive_chain", SubprocessDispatch { interpreter: None });

    // Common interpreter-specific terminals
    reg.register(
        "@python3",
        SubprocessDispatch {
            interpreter: Some("python3".into()),
        },
    );
    reg.register(
        "@node",
        SubprocessDispatch {
            interpreter: Some("node".into()),
        },
    );
    reg.register(
        "@bash",
        SubprocessDispatch {
            interpreter: Some("bash".into()),
        },
    );

    reg
}

/// Discover the user-space root (parent of `~/.ai/`).
fn discover_user_root() -> Option<PathBuf> {
    std::env::var_os("USER_SPACE")
        .map(PathBuf::from)
        .or_else(|| BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
}
