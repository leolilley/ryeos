//! Shared engine boot utilities for `rye-inspect` subcommands.
//!
//! Each subcommand (fetch, verify, identity) needs the same engine
//! infrastructure: `TrustStore`, `KindRegistry`, `ParserDispatcher`.
//! This module provides a single `boot()` that builds a fully-loaded
//! `Engine`, reusing the same discovery logic as `rye-sign` and the daemon.

pub mod fetch;
pub mod identity;
pub mod verify;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use ryeos_engine::engine::Engine;
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::parsers::{NativeParserHandlerRegistry, ParserDispatcher, ParserRegistry};
use ryeos_engine::trust::TrustStore;

/// Boot the engine from the same sources the daemon uses.
pub fn boot(project_path: Option<&Path>) -> Result<Engine> {
    let user_root = dirs::home_dir();
    let system_roots = discover_system_roots();

    let trust_store = TrustStore::load_three_tier(
        project_path,
        user_root.as_deref(),
        &system_roots,
    )
    .with_context(|| "load trust store")?;

    let kinds = build_kind_registry(&system_roots, &trust_store)?;
    let parsers = build_parser_dispatcher(
        &system_roots,
        user_root.as_deref(),
        &kinds,
        &trust_store,
    )?;

    Ok(Engine::new(kinds, parsers, user_root, system_roots)
        .with_trust_store(trust_store))
}

fn build_kind_registry(
    system_roots: &[PathBuf],
    trust_store: &TrustStore,
) -> Result<KindRegistry> {
    let mut search = Vec::new();
    for r in system_roots {
        let p = r.join(".ai").join("node").join("engine").join("kinds");
        if p.exists() {
            search.push(p);
        }
    }
    KindRegistry::load_base(&search, trust_store).with_context(|| "load kind registry")
}

fn build_parser_dispatcher(
    system_roots: &[PathBuf],
    user_root: Option<&Path>,
    kinds: &KindRegistry,
    trust_store: &TrustStore,
) -> Result<ParserDispatcher> {
    let mut search: Vec<PathBuf> = system_roots.to_vec();
    if let Some(u) = user_root {
        search.push(u.to_path_buf());
    }
    let (parser_tools, _) = ParserRegistry::load_base(&search, trust_store, kinds)
        .with_context(|| "load parser tool descriptors")?;
    let native_handlers = NativeParserHandlerRegistry::with_builtins();
    Ok(ParserDispatcher::new(parser_tools, native_handlers))
}

/// Discover system bundle roots. Mirrors `sign.rs::discover_system_roots`.
fn discover_system_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if let Ok(p) = std::env::var("RYE_SYSTEM_SPACE") {
        roots.push(PathBuf::from(p));
    }

    let state_dir = match std::env::var("RYEOS_STATE_DIR") {
        Ok(p) => PathBuf::from(p),
        Err(_) => dirs::state_dir()
            .map(|d| d.join("ryeosd"))
            .unwrap_or_else(|| PathBuf::from(".ryeosd")),
    };
    let bundles_dir = state_dir.join(".ai").join("bundles");

    if let Ok(entries) = std::fs::read_dir(&bundles_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                roots.push(entry.path());
            }
        }
    }
    roots.sort();
    roots
}
