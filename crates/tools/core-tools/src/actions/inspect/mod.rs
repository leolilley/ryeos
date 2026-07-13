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
    let operator_config_root = roots::runtime_root().ok().map(|root| root.config());
    // Match daemon boot semantics: content roots are the installed bundle
    // roots only. `RYEOS_APP_ROOT` is the daemon runtime state dir that
    // contains registrations and runtime state; treating it as a content
    // root makes effective items report the state dir as their bundle_root,
    // which breaks bundle-local binary resolution for client launchers.
    let bundle_roots = discover_bundle_roots();

    let trust_store = TrustStore::load(
        project_path,
        operator_config_root
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve app root"))?,
    )
    .with_context(|| "load trust store")?;

    let kinds = build_kind_registry(&bundle_roots, &trust_store)?;
    let (parsers, handlers) = build_parser_dispatcher(&bundle_roots, &kinds, &trust_store)?;
    let composers = ComposerRegistry::from_kinds(&kinds, &handlers)
        .with_context(|| "load composer registry")?;

    Ok(Engine::new(kinds, parsers, bundle_roots)
        .with_trust_store(trust_store)
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
    let handlers = HandlerRegistry::load_base(&tagged_search, trust_store)
        .with_context(|| "load handler descriptors")?;
    let handlers = Arc::new(handlers);
    Ok((
        ParserDispatcher::new(parser_tools, handlers.clone()),
        handlers,
    ))
}

/// Discover installed bundle roots from the daemon state directory.
fn discover_bundle_roots() -> Vec<PathBuf> {
    let app_root = match std::env::var("RYEOS_APP_ROOT") {
        Ok(p) => PathBuf::from(p),
        Err(_) => dirs::data_dir()
            .map(|d| d.join("ryeos"))
            .expect("could not determine XDG data directory"),
    };
    let mut roots = Vec::new();
    let ai_dir = app_root.join(ryeos_engine::AI_DIR);
    // When RYEOS_APP_ROOT is itself a bundle tree (it carries a signed
    // `.ai/manifest.yaml`), it IS the content root and the installed-bundle
    // layout (`.ai/bundles/*`) is absent. Include it so single-bundle app
    // roots resolve.
    if ai_dir.join("manifest.yaml").is_file() {
        roots.push(app_root.clone());
    }
    let bundles_dir = ai_dir.join("bundles");
    if let Ok(entries) = std::fs::read_dir(&bundles_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                roots.push(entry.path());
            }
        }
    }
    roots
}
