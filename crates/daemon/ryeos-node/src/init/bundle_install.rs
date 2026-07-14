//! Bundle filesystem installation and atomic replacement mechanics used by init.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

/// Verify that an existing bundle directory has the expected `.ai/` structure.
pub(super) fn verify_bundle_structure(target: &Path) -> Result<()> {
    if !target.join(ryeos_engine::AI_DIR).is_dir() {
        bail!(
            "{} exists but is not a Rye bundle — refusing to clobber",
            target.display()
        );
    }
    Ok(())
}

/// Atomically replace an installed bundle with a new version.
///
/// Instead of copying on top (which leaves stale files), this:
/// 1. Copies source to a staging directory
/// 2. Atomically exchanges staging with the installed path
/// 3. Removes the old generation now located at staging
pub(super) fn replace_bundle(
    source: &Path,
    target: &Path,
    transaction: &ryeos_app::bundle_transaction::BundleTransaction,
    registration: serde_json::Value,
    validate_staging: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("bundle path has no parent"))?;
    let name = target
        .file_name()
        .ok_or_else(|| anyhow!("bundle path has no name"))?
        .to_string_lossy();

    let staging = parent.join(format!(".{name}.staging"));
    (|| {
        if staging.exists() {
            fs::remove_dir_all(&staging)
                .with_context(|| format!("clean up stale staging {}", staging.display()))?;
        }
        copy_dir_recursive(source, &staging)
            .with_context(|| format!("stage {} -> {}", source.display(), staging.display()))?;
        // Re-read the completed copy rather than trusting the earlier source
        // preflight; this catches source changes that raced the copy.
        validate_staging(&staging)?;
        lillux::sync_tree_durable(&staging)
            .with_context(|| format!("flush staged bundle {}", staging.display()))?;
        transaction.begin_present(
            ryeos_app::bundle_transaction::BundleOperation::Replace,
            &staging,
            registration,
        )?;
        lillux::atomic_exchange_paths(target, &staging).with_context(|| {
            format!(
                "atomically exchange installed bundle {} with {}",
                target.display(),
                staging.display()
            )
        })?;
        transaction.mark_activated()?;
        if let Err(error) = lillux::remove_dir_all_durable(&staging) {
            tracing::warn!(
                path = %staging.display(),
                error = %error,
                "bundle replacement committed but previous generation cleanup failed"
            );
        }
        Ok(())
    })()
}

/// Install a bundle by copy + signed `kind: node` registration.
///
/// Mirrors `service:bundle/install` semantics but runs in-process (no daemon
/// required). Init verifies the complete source set before calling this helper;
/// `validate_staging` re-verifies the completed copy before activation.
///
/// Returns the canonical path of the installed bundle.
pub(super) fn install_bundle(
    app_root: &Path,
    name: &str,
    source: &Path,
    transaction: &ryeos_app::bundle_transaction::BundleTransaction,
    registration: serde_json::Value,
    validate_staging: impl FnOnce(&Path) -> Result<()>,
) -> Result<PathBuf> {
    // Copy bundle into <app_root>/.ai/bundles/<name>/
    let target = app_root
        .join(ryeos_engine::AI_DIR)
        .join("bundles")
        .join(name);
    let parent = target
        .parent()
        .context("bundle install target has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create bundles parent for {}", target.display()))?;
    let staging = parent.join(format!(".{name}.staging"));
    (|| {
        if target.exists() {
            bail!(
                "bundle target appeared during install: {}",
                target.display()
            );
        }
        if staging.exists() {
            fs::remove_dir_all(&staging)
                .with_context(|| format!("remove stale staging {}", staging.display()))?;
        }
        copy_dir_recursive(source, &staging)
            .with_context(|| format!("stage {} at {}", name, staging.display()))?;
        // Re-read the completed copy rather than trusting the earlier source
        // preflight; this catches source changes that raced the copy.
        validate_staging(&staging)?;
        lillux::sync_tree_durable(&staging)
            .with_context(|| format!("flush staged bundle {}", staging.display()))?;
        transaction.begin_present(
            ryeos_app::bundle_transaction::BundleOperation::Install,
            &staging,
            registration,
        )?;
        lillux::rename_path_durable(&staging, &target)
            .with_context(|| format!("activate {} at {}", name, target.display()))?;
        transaction.mark_activated()
    })()?;
    let canonical = target
        .canonicalize()
        .with_context(|| format!("canonicalize {} install path", name))?;

    Ok(canonical)
}

pub(super) fn bundle_registration_value(
    path: &Path,
    command_registration_caps: &[String],
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "kind": "node",
        "path": path,
    });
    if !command_registration_caps.is_empty() {
        value["command_registration_caps"] = serde_json::json!(command_registration_caps);
    }
    value
}

/// Recursive directory copy with symlink preservation (Unix only).
pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).with_context(|| format!("create {}", dst.display()))?;
    for entry in fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if file_type.is_symlink() {
            let link_target = fs::read_link(&from)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link_target, &to)
                .with_context(|| format!("symlink {}", to.display()))?;
            #[cfg(not(unix))]
            {
                let _ = link_target;
                bail!("symlinks unsupported on this platform: {}", from.display());
            }
        } else {
            fs::copy(&from, &to)
                .with_context(|| format!("copy {} -> {}", from.display(), to.display()))?;
            // Preserve the source mtime (best-effort): bundle verification
            // includes an mtime-based manifest-freshness check
            // (manifest.yaml must not be older than manifest.source.yaml),
            // and a copy stamping fresh mtimes in directory-iteration order
            // can invert that relationship on the installed tree — a
            // millisecond of copy-order skew then reads as a stale manifest.
            if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                if let Ok(file) = fs::File::options().write(true).open(&to) {
                    let _ = file.set_modified(modified);
                }
            }
        }
    }
    Ok(())
}
