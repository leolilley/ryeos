//! Stopped-node, exact legacy product-qualification head retirement.
//!
//! This command cannot create current qualification or alter CAS bytes. The
//! full copied state archive is verified before the first durable intent.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use serde::Serialize;

use ryeos_node_runtime::config::{Config, ConfigSources};
use ryeos_node_runtime::state_lock::{StateLock, default_lock_path};

#[derive(Debug, Clone, Default)]
pub struct LegacyProductQualificationRetirementOptions {
    pub app_root: Option<PathBuf>,
    pub dry_run: bool,
    pub expected_plan_sha256: Option<String>,
    pub archive_state_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LegacyProductQualificationRetirementReport {
    pub app_root: PathBuf,
    pub dry_run: bool,
    pub performed: bool,
    pub preview: ryeos_state::state_db::LegacyQualificationRetirementPreview,
    pub archive: Option<ryeos_state::state_db::LegacyQualificationArchiveReceipt>,
}

pub fn run_legacy_product_qualification_retirement(
    options: &LegacyProductQualificationRetirementOptions,
) -> Result<LegacyProductQualificationRetirementReport> {
    crate::object_contracts::install()
        .context("install current object contracts for qualification retirement")?;
    let config = Config::load(&ConfigSources {
        app_root: options.app_root.clone(),
        ..ConfigSources::default()
    })?;
    let state_lock_path = default_lock_path(&config.app_root);
    let _state_lock = StateLock::acquire_existing_read_only(&state_lock_path)
        .context("qualification retirement requires the daemon to be stopped")?;
    let runtime_state_dir = config.runtime_state_dir();
    let runtime_directory =
        lillux::PinnedDirectory::open(&runtime_state_dir)?.context("runtime state is absent")?;
    let runtime_lock = runtime_directory
        .lock_exclusive()
        .context("lock offline runtime-state namespace")?;
    let identity = ryeos_node_runtime::identity::NodeIdentity::load(&config.node_signing_key_path)?;
    let mut trust = ryeos_state::refs::TrustStore::new();
    trust.insert(
        identity.fingerprint().to_string(),
        *identity.verifying_key(),
    );
    let state = ryeos_state::StateDb::open_for_legacy_qualification_retirement(
        &runtime_state_dir,
        Arc::new(trust),
    )?;
    let authority = state.pinned_authority()?;
    ensure!(
        authority
            .runtime_directory()
            .is_same_directory(&runtime_directory)?,
        "runtime-state path changed while qualification retirement captured authority"
    );
    let _runtime_lock = runtime_lock;
    let guard = authority.acquire_exclusive_guard(false)?;
    let preview =
        state.preview_legacy_product_qualification_retirement(identity.verifying_key(), &guard)?;
    if options.dry_run {
        return Ok(LegacyProductQualificationRetirementReport {
            app_root: config.app_root,
            dry_run: true,
            performed: false,
            preview,
            archive: None,
        });
    }
    let expected = options
        .expected_plan_sha256
        .as_deref()
        .context("qualification retirement requires the reviewed exact plan SHA-256")?;
    ensure!(
        preview.plan_sha256 == expected,
        "qualification retirement plan differs from reviewed dry-run"
    );
    let archive_root = options
        .archive_state_root
        .as_deref()
        .context("qualification retirement requires a separate complete state archive")?;
    let archive = state.retire_legacy_product_qualification_heads(
        identity.verifying_key(),
        expected,
        archive_root,
        &guard,
    )?;
    Ok(LegacyProductQualificationRetirementReport {
        app_root: config.app_root,
        dry_run: false,
        performed: true,
        preview,
        archive: Some(archive),
    })
}
