//! Shared engine boot utilities for `ryeos-core-tools` subcommands.
//!
//! Each subcommand (fetch, verify, identity) needs the same engine
//! infrastructure: `TrustStore`, `KindRegistry`, `ParserDispatcher`.
//! This module provides a single `boot()` that builds a fully-loaded
//! `Engine`, reusing the same discovery logic as `ryeos-core-tools` and the daemon.

pub mod fetch;
pub mod identity;
pub mod verify;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use ryeos_engine::composers::ComposerRegistry;
use ryeos_engine::engine::Engine;
use ryeos_engine::kind_registry::KindRegistry;
use std::sync::Arc;

use ryeos_engine::handlers::HandlerRegistry;
use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};
use ryeos_engine::roots;
use ryeos_engine::trust::TrustStore;

/// Boot the engine from the same sources the daemon uses.
pub fn boot(project_path: Option<&Path>) -> Result<Engine> {
    let app_root = roots::app_root().context("resolve app root for node isolation policy")?;
    let isolation = ryeos_app::engine_init::load_locked_registered_isolation(&app_root)
        .context("load retained node isolation generation")?;
    let bundle_roots = isolation
        .registered_generation_bundle_roots()
        .context("retained isolation generation omitted bundle roots")?
        .to_vec();
    let node_trust_store = isolation
        .registered_generation_node_trust()
        .context("retained isolation generation omitted node trust")?
        .clone();
    let trust_store = match project_path {
        Some(project_path) => node_trust_store
            .with_project_keys(project_path)
            .map(std::borrow::Cow::into_owned)
            .with_context(|| "load project item trust")?,
        None => node_trust_store.clone(),
    };
    let kinds = build_kind_registry(&bundle_roots, &node_trust_store)?;
    let (parsers, handlers) =
        build_parser_dispatcher(&bundle_roots, &kinds, &node_trust_store, isolation)?;
    let composers = ComposerRegistry::from_kinds(&kinds, &handlers)
        .with_context(|| "load composer registry")?;

    Ok(Engine::new(kinds, parsers, bundle_roots)
        .with_trust_store(trust_store)
        .with_node_trust_store(node_trust_store)
        .with_composers(composers))
}

fn build_kind_registry(bundle_roots: &[PathBuf], trust_store: &TrustStore) -> Result<KindRegistry> {
    let mut search = Vec::new();
    for r in bundle_roots {
        let p = r.join(".ai").join("node").join("engine").join("kinds");
        if p.exists() {
            search.push(p);
        }
    }
    KindRegistry::load_base(&search, trust_store).with_context(|| "load kind registry")
}

fn build_parser_dispatcher(
    bundle_roots: &[PathBuf],
    kinds: &KindRegistry,
    trust_store: &TrustStore,
    isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
) -> Result<(ParserDispatcher, Arc<HandlerRegistry>)> {
    let search: Vec<PathBuf> = bundle_roots.to_vec();
    let tagged_search: Vec<(PathBuf, ryeos_engine::resolution::TrustClass)> = bundle_roots
        .iter()
        .map(|r| {
            (
                r.clone(),
                ryeos_engine::resolution::TrustClass::TrustedBundle,
            )
        })
        .collect();
    let (parser_tools, _) = ParserRegistry::load_base(&search, trust_store, kinds)
        .with_context(|| "load parser tool descriptors")?;
    let handlers = HandlerRegistry::load_base(&tagged_search, trust_store, isolation)
        .with_context(|| "load handler descriptors")?;
    let handlers = Arc::new(handlers);
    Ok((
        ParserDispatcher::new(parser_tools, handlers.clone()),
        handlers,
    ))
}
