//! `bundle.install` — install a downstream bundle via node-config writer.
//!
//! Copies source to `<state_dir>/.ai/bundles/<name>/`, writes a signed
//! `kind: node` `section: bundles` item at `<state_dir>/.ai/node/bundles/<name>.yaml`.
//!
//! `core` cannot be installed — it is the packaged base root laid down at
//! `system_data_dir` out-of-band (by the OS package installer or manual copy).
//!
//! OfflineOnly: the daemon must be stopped (engine reload not implemented).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use ryeos_engine::roots;
use ryeos_tools::actions::install::preflight_verify_bundle;

use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::state::AppState;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Bundle name; becomes the install directory name.
    pub name: String,
    /// Source directory to copy from.
    pub source_path: PathBuf,
}

fn validate_name(name: &str) -> Result<()> {
    if name == "core" {
        bail!(
            "\"core\" cannot be installed via bundle.install — \
             it is the packaged base root and must be laid down at \
             system_data_dir out-of-band"
        );
    }
    if name.is_empty()
        || name
            .contains(|c: char| c == '/' || c == '\\' || c == '.' || c.is_whitespace())
    {
        bail!(
            "invalid bundle name '{}': must be non-empty and contain no path separators, \
             dots, or whitespace",
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

    let bundles_root = state.config.state_dir.join(".ai").join("bundles");
    let target = bundles_root.join(&req.name);

    if target.exists() {
        bail!(
            "bundle '{}' already installed at {}",
            req.name,
            target.display()
        );
    }

    // Preflight verification: parse + validate + signature-check every
    // signable item in the bundle BEFORE any filesystem mutation.
    //
    // Trust source: operator-tier ONLY (project + user). System bundles
    // (`system_data_dir`) contribute kind schemas + parser tools, never
    // trust docs. Bundles whose signers aren't already trusted are
    // rejected — operators must `rye trust pin <fingerprint>` first.
    let user_root = roots::user_root().ok();
    preflight_verify_bundle(
        &req.source_path,
        &state.config.system_data_dir,
        user_root.as_deref(),
    )
    .context("preflight verification refused install")?;

    fs::create_dir_all(&bundles_root).with_context(|| {
        format!(
            "failed to create bundles root {}",
            bundles_root.display()
        )
    })?;

    copy_dir_recursive(&req.source_path, &target).with_context(|| {
        format!(
            "failed to copy bundle from {} to {}",
            req.source_path.display(),
            target.display()
        )
    })?;

    let canonical_target = target
        .canonicalize()
        .context("failed to canonicalize installed bundle path")?;

    // Write signed kind: node bundle registration
    let config_item_path = crate::node_config::writer::write_signed_node_item(
        &state.config.state_dir.join(".ai").join("node"),
        "bundles",
        &req.name,
        &serde_json::json!({ "path": canonical_target }),
        &state.identity,
    )?;

    let report = serde_json::json!({
        "name": req.name,
        "path": canonical_target.display().to_string(),
        "config_item": config_item_path.display().to_string(),
    });
    Ok(report)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)
        .with_context(|| format!("failed to create {}", dst.display()))?;
    for entry in fs::read_dir(src)
        .with_context(|| format!("failed to read {}", src.display()))?
    {
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
            fs::copy(&from, &to)
                .with_context(|| format!("failed to copy {} -> {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle/install",
    endpoint: "bundle.install",
    availability: ServiceAvailability::OfflineOnly,
    required_caps: &["node.maintenance"],
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)
                .context("bundle.install requires { name, source_path }")?;
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
    fn validate_name_accepts_valid() {
        assert!(validate_name("my-bundle_v2").is_ok());
    }

    #[test]
    fn validate_name_rejects_core() {
        let err = validate_name("core").unwrap_err();
        assert!(
            err.to_string().contains("cannot be installed via bundle.install"),
            "expected core refusal, got: {err}"
        );
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
}
