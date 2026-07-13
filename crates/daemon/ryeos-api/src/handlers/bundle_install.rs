//! `bundle.install` — install a downstream bundle via node-config writer.
//!
//! Copies source to `<app_root>/.ai/bundles/<name>/`, writes a signed
//! `kind: node` bundle registration item at `<app_root>/.ai/node/bundles/<name>.yaml`.
//!
//! Any bundle name is accepted — no special treatment for any name.
//!
//! OfflineOnly: the daemon must be stopped (engine reload not implemented).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use ryeos_bundle::preflight::preflight_verify_bundle_in_context;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Bundle name; becomes the install directory name.
    pub name: String,
    /// Source directory to copy from.
    pub source_path: PathBuf,
    /// Replace an existing installed bundle after preflight verification.
    #[serde(default)]
    pub replace: bool,
    /// Preserve installed runtime artifacts when replacing from a sparse source.
    ///
    /// This keeps `.ai/bin`, `.ai/objects`, and `.ai/refs` from the existing
    /// installation when the replacement source does not provide them. Source
    /// files always win when present.
    #[serde(default)]
    pub preserve_runtime_artifacts: bool,
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("invalid bundle name '{}': must be 1–64 characters", name);
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        bail!(
            "invalid bundle name '{}': must contain only lowercase letters, digits, underscore, or hyphen",
            name
        );
    }
    Ok(())
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    validate_name(&req.name)?;

    if !req.source_path.is_dir() {
        bail!(
            "source_path is not a directory: {}",
            req.source_path.display()
        );
    }

    let bundles_root = state.config.app_root.join(".ai").join("bundles");
    let target = bundles_root.join(&req.name);

    let replaced = target.exists();
    if replaced && !req.replace {
        bail!(
            "bundle '{}' already installed at {}; use --replace to update it from source",
            req.name,
            target.display()
        );
    }

    // Preflight verification: parse + validate + signature-check every
    // signable item in the bundle BEFORE any filesystem mutation.
    //
    // Trust source: project + operator app root. Bundles whose signers aren't
    // already trusted are rejected — operators must pin trust first.
    let operator_config_root = state.config.runtime_root().config();
    let source_canonical = req
        .source_path
        .canonicalize()
        .with_context(|| format!("canonicalize source path {}", req.source_path.display()))?;
    let installed_dependency_roots: Vec<PathBuf> =
        ryeos_bundle::installed::load_installed_bundle_records(&state.config.app_root)
            .context("preflight: load installed bundle registrations")?
            .into_iter()
            .filter(|record| record.name != req.name && record.bundle_root != source_canonical)
            .map(|record| record.bundle_root)
            .collect();
    preflight_verify_bundle_in_context(
        &req.source_path,
        &installed_dependency_roots,
        &operator_config_root,
    )
    .context("preflight verification refused install")?;

    let node_dir = state.config.app_root.join(".ai").join("node");
    // Replacement keeps the same canonical path. Publish/repair its signed
    // registration before exchanging trees, so either the old or new complete
    // generation is always registered.
    let existing_registration = if replaced {
        Some(ryeos_app::node_config::writer::write_signed_node_item(
            &node_dir,
            "bundles",
            &req.name,
            &serde_json::json!({ "path": target.canonicalize()? }),
            &state.identity,
        )?)
    } else {
        None
    };

    fs::create_dir_all(&bundles_root)
        .with_context(|| format!("failed to create bundles root {}", bundles_root.display()))?;

    if replaced {
        replace_dir_atomic(&req.source_path, &target, req.preserve_runtime_artifacts)
            .with_context(|| {
                format!(
                    "failed to replace bundle from {} to {}",
                    req.source_path.display(),
                    target.display()
                )
            })?;
    } else {
        install_dir_atomic(&req.source_path, &target).with_context(|| {
            format!(
                "failed to install bundle from {} to {}",
                req.source_path.display(),
                target.display()
            )
        })?;
    }

    let canonical_target = target
        .canonicalize()
        .context("failed to canonicalize installed bundle path")?;

    // Write signed kind: node bundle registration
    let config_item_path = match existing_registration {
        Some(path) => path,
        None => match ryeos_app::node_config::writer::write_signed_node_item(
            &node_dir,
            "bundles",
            &req.name,
            &serde_json::json!({ "path": canonical_target }),
            &state.identity,
        ) {
            Ok(path) => path,
            Err(error) => {
                lillux::with_exclusive_file_lock(&target, || {
                    if target.exists() {
                        fs::remove_dir_all(&target).with_context(|| {
                            format!("remove unregistered bundle {}", target.display())
                        })?;
                    }
                    Ok(())
                })?;
                return Err(error).context("write bundle registration");
            }
        },
    };

    // Bump the engine cache generation so any cached per-request
    // engines (built against the previous bundle set) are invalidated.
    // The next pushed_head request will build a fresh engine that
    // includes the newly installed bundle.
    let new_gen = state.engine_cache.bump_system_install_generation();
    tracing::info!(
        bundle = %req.name,
        engine_cache_generation = new_gen,
        "bundle installed: bumped engine cache generation"
    );

    let report = serde_json::json!({
        "name": req.name,
        "path": canonical_target.display().to_string(),
        "config_item": config_item_path.display().to_string(),
        "replaced": replaced,
        "preserve_runtime_artifacts": req.preserve_runtime_artifacts,
    });
    Ok(report)
}

fn install_dir_atomic(src: &Path, target: &Path) -> Result<()> {
    let parent = target
        .parent()
        .context("installed bundle target has no parent")?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .context("installed bundle target has no valid directory name")?;
    let staging = parent.join(format!(".{name}.staging"));
    lillux::with_exclusive_file_lock(target, || {
        if target.exists() {
            bail!(
                "bundle target appeared during install: {}",
                target.display()
            );
        }
        if staging.exists() {
            fs::remove_dir_all(&staging)
                .with_context(|| format!("remove stale staging dir {}", staging.display()))?;
        }
        copy_dir_recursive(src, &staging).with_context(|| {
            format!(
                "copy bundle from {} to staging {}",
                src.display(),
                staging.display()
            )
        })?;
        lillux::rename_path_durable(&staging, target).with_context(|| {
            format!(
                "activate staged bundle {} at {}",
                staging.display(),
                target.display()
            )
        })
    })
}

fn replace_dir_atomic(src: &Path, target: &Path, preserve_runtime_artifacts: bool) -> Result<()> {
    let source = src
        .canonicalize()
        .with_context(|| format!("canonicalize source path {}", src.display()))?;
    let canonical_target = target
        .canonicalize()
        .with_context(|| format!("canonicalize target path {}", target.display()))?;
    let parent = target
        .parent()
        .context("installed bundle target has no parent")?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .context("installed bundle target has no valid directory name")?;
    let staging = parent.join(format!(".{name}.staging"));
    if source == canonical_target
        || source.starts_with(&canonical_target)
        || canonical_target.starts_with(&source)
    {
        bail!(
            "source_path {} must be outside installed bundle target {} when using --replace",
            source.display(),
            canonical_target.display()
        );
    }
    if staging.starts_with(&source) {
        bail!(
            "source_path {} must not contain the bundle install staging path",
            source.display()
        );
    }

    lillux::with_exclusive_file_lock(target, || {
        if staging.exists() {
            fs::remove_dir_all(&staging)
                .with_context(|| format!("remove stale staging dir {}", staging.display()))?;
        }
        copy_dir_recursive(src, &staging).with_context(|| {
            format!(
                "copy replacement bundle from {} to staging {}",
                src.display(),
                staging.display(),
            )
        })?;
        if preserve_runtime_artifacts {
            preserve_runtime_artifact_dirs(&canonical_target, &staging)?;
        }
        lillux::atomic_exchange_paths(target, &staging).with_context(|| {
            format!(
                "atomically exchange installed bundle {} with {}",
                target.display(),
                staging.display()
            )
        })?;
        fs::remove_dir_all(&staging)
            .with_context(|| format!("remove previous bundle generation {}", staging.display()))
    })
}

fn preserve_runtime_artifact_dirs(existing: &Path, staging: &Path) -> Result<()> {
    for rel in [".ai/bin", ".ai/objects", ".ai/refs"] {
        let from = existing.join(rel);
        if !from.exists() {
            continue;
        }
        let to = staging.join(rel);
        if to.exists() {
            continue;
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create runtime artifact parent {}", parent.display()))?;
        }
        copy_dir_recursive(&from, &to).with_context(|| {
            format!(
                "preserve runtime artifact directory {} -> {}",
                from.display(),
                to.display()
            )
        })?;
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).with_context(|| format!("failed to create {}", dst.display()))?;
    for entry in fs::read_dir(src).with_context(|| format!("failed to read {}", src.display()))? {
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
                .with_context(|| format!("failed to symlink {}", to.display()))?;
            #[cfg(not(unix))]
            {
                let _ = link_target;
                bail!("symlinks unsupported on this platform: {}", from.display());
            }
        } else {
            fs::copy(&from, &to).with_context(|| {
                format!("failed to copy {} -> {}", from.display(), to.display())
            })?;
        }
    }
    Ok(())
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle/install",
    endpoint: "bundle.install",
    availability: ServiceAvailability::OfflineOnly,
    required_caps: &["ryeos.execute.service.bundle/install"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)
                .context("bundle.install requires { name, source_path, replace? }")?;
            handle(req, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_name_rejects_empty() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn validate_name_rejects_slashes() {
        assert!(validate_name("foo/bar").is_err());
    }

    #[test]
    fn validate_name_rejects_dots() {
        assert!(validate_name("foo.bar").is_err());
    }

    #[test]
    fn validate_name_rejects_uppercase() {
        assert!(validate_name("Foo").is_err());
    }

    #[test]
    fn validate_name_rejects_spaces() {
        assert!(validate_name("foo bar").is_err());
    }

    #[test]
    fn validate_name_accepts_valid() {
        assert!(validate_name("my-bundle_v2").is_ok());
        assert!(validate_name("core").is_ok());
        assert!(validate_name("standard").is_ok());
    }

    #[test]
    fn copy_dir_copies_nested_files() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(src.join("a/b")).unwrap();
        fs::write(src.join("top.txt"), b"top").unwrap();
        fs::write(src.join("a/mid.txt"), b"mid").unwrap();
        fs::write(src.join("a/b/leaf.txt"), b"leaf").unwrap();

        copy_dir_recursive(&src, &dst).unwrap();

        assert_eq!(fs::read(dst.join("top.txt")).unwrap(), b"top");
        assert_eq!(fs::read(dst.join("a/mid.txt")).unwrap(), b"mid");
        assert_eq!(fs::read(dst.join("a/b/leaf.txt")).unwrap(), b"leaf");
    }

    #[test]
    fn replace_dir_replaces_existing_tree_and_cleans_staging() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let bundles = tmp.path().join("bundles");
        let target = bundles.join("ryeos-ui");
        fs::create_dir_all(src.join("new")).unwrap();
        fs::create_dir_all(target.join("old")).unwrap();
        fs::write(src.join("new/file.txt"), b"new").unwrap();
        fs::write(target.join("old/file.txt"), b"old").unwrap();

        replace_dir_atomic(&src, &target, false).unwrap();

        assert_eq!(fs::read(target.join("new/file.txt")).unwrap(), b"new");
        assert!(!target.join("old/file.txt").exists());
        assert!(!bundles.join(".ryeos-ui.staging").exists());
        assert!(!bundles.join(".ryeos-ui.staging").exists());
    }

    #[test]
    fn replace_dir_preserves_runtime_artifacts_when_source_is_sparse() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let bundles = tmp.path().join("bundles");
        let target = bundles.join("ryeos-ui");
        fs::create_dir_all(src.join(".ai/node/commands")).unwrap();
        fs::create_dir_all(target.join(".ai/bin/x86_64-unknown-linux-gnu")).unwrap();
        fs::create_dir_all(target.join(".ai/objects/blobs")).unwrap();
        fs::create_dir_all(target.join(".ai/refs/bundles")).unwrap();
        fs::write(src.join(".ai/node/commands/web.yaml"), b"web").unwrap();
        fs::write(target.join(".ai/bin/x86_64-unknown-linux-gnu/web"), b"bin").unwrap();
        fs::write(target.join(".ai/objects/blobs/blob"), b"blob").unwrap();
        fs::write(target.join(".ai/refs/bundles/manifest"), b"ref").unwrap();

        replace_dir_atomic(&src, &target, true).unwrap();

        assert_eq!(
            fs::read(target.join(".ai/node/commands/web.yaml")).unwrap(),
            b"web"
        );
        assert_eq!(
            fs::read(target.join(".ai/bin/x86_64-unknown-linux-gnu/web")).unwrap(),
            b"bin"
        );
        assert_eq!(
            fs::read(target.join(".ai/objects/blobs/blob")).unwrap(),
            b"blob"
        );
        assert_eq!(
            fs::read(target.join(".ai/refs/bundles/manifest")).unwrap(),
            b"ref"
        );
    }

    #[test]
    fn replace_dir_does_not_overwrite_source_runtime_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let bundles = tmp.path().join("bundles");
        let target = bundles.join("ryeos-ui");
        fs::create_dir_all(src.join(".ai/bin/x86_64-unknown-linux-gnu")).unwrap();
        fs::create_dir_all(target.join(".ai/bin/x86_64-unknown-linux-gnu")).unwrap();
        fs::write(src.join(".ai/bin/x86_64-unknown-linux-gnu/web"), b"new").unwrap();
        fs::write(target.join(".ai/bin/x86_64-unknown-linux-gnu/web"), b"old").unwrap();

        replace_dir_atomic(&src, &target, true).unwrap();

        assert_eq!(
            fs::read(target.join(".ai/bin/x86_64-unknown-linux-gnu/web")).unwrap(),
            b"new"
        );
    }
}
