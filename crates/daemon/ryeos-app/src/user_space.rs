//! User-space path and YAML helpers for Studio/user-principal state.
//!
//! This module is the local-install seam for future principal/tenant-aware
//! resolution. Callers should use logical `config/*` and `state/*` paths here
//! instead of constructing `<user_root>/.ai/...` ad hoc.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};

/// Synthetic principal for the current local single-user install.
///
/// Hosted/multi-principal mode should derive a real principal from the
/// authenticated caller and resolve through [`UserSpaceResolver`] without
/// changing callers that operate on [`UserSpacePaths`].
pub const LOCAL_PRINCIPAL_ID: &str = "local";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSpacePaths {
    pub root: PathBuf,
}

impl UserSpacePaths {
    pub fn resolve() -> Result<Self> {
        LocalUserSpaceResolver.resolve(LOCAL_PRINCIPAL_ID)
    }

    fn resolve_local() -> Result<Self> {
        let root = ryeos_engine::roots::user_root().context("failed to resolve user space root")?;
        Ok(Self { root })
    }

    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn config(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.root.join(".ai").join("config").join(rel.as_ref())
    }

    pub fn state(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.root.join(".ai").join("state").join(rel.as_ref())
    }

    pub fn projects_config(&self) -> PathBuf {
        self.config("projects.yaml")
    }

    pub fn studio_config(&self) -> PathBuf {
        self.config("studio.yaml")
    }

    pub fn studio_recent(&self) -> PathBuf {
        self.state("studio/recent.yaml")
    }
}

/// Resolves logical user-space storage for a principal.
///
/// Local RyeOS maps every caller to the same local user space. Future hosted
/// mode can replace this with a resolver backed by per-principal filesystem,
/// database, or object storage while preserving the logical config/state paths.
pub trait UserSpaceResolver {
    fn resolve(&self, principal_id: &str) -> Result<UserSpacePaths>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LocalUserSpaceResolver;

impl UserSpaceResolver for LocalUserSpaceResolver {
    fn resolve(&self, principal_id: &str) -> Result<UserSpacePaths> {
        if principal_id.trim().is_empty() {
            anyhow::bail!("principal id is required to resolve user space");
        }
        UserSpacePaths::resolve_local()
    }
}

pub fn read_yaml_or_default<T>(path: &Path) -> Result<T>
where
    T: DeserializeOwned + Default,
{
    if !path.exists() {
        return Ok(T::default());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_yaml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

pub fn write_yaml_atomic<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    ensure_private_parent_dirs(path)?;
    let body = serde_yaml::to_string(value)
        .with_context(|| format!("failed to serialize {}", path.display()))?;
    crate::io::atomic::atomic_write(path, body.as_bytes())?;
    set_private_file_permissions(path)?;
    Ok(())
}

fn ensure_private_parent_dirs(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent dir {}", parent.display()))?;
        let mut dirs = Vec::new();
        let mut current = Some(parent);
        let mut found_ai_dir = false;
        while let Some(dir) = current {
            dirs.push(dir);
            if dir.file_name().is_some_and(|name| name == ".ai") {
                found_ai_dir = true;
                break;
            }
            current = dir.parent();
        }
        let dirs_to_chmod: Vec<&Path> = if found_ai_dir {
            dirs.into_iter().rev().collect()
        } else {
            vec![parent]
        };
        for dir in dirs_to_chmod {
            set_private_dir_permissions(dir)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to chmod 0700 {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to chmod 0600 {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[derive(Debug, Default, Serialize, serde::Deserialize, PartialEq)]
    struct Demo {
        value: String,
    }

    #[test]
    fn logical_paths_live_under_user_ai_config_and_state() {
        let paths = UserSpacePaths::new(PathBuf::from("/tmp/user"));
        assert_eq!(
            paths.projects_config(),
            PathBuf::from("/tmp/user/.ai/config/projects.yaml")
        );
        assert_eq!(
            paths.studio_recent(),
            PathBuf::from("/tmp/user/.ai/state/studio/recent.yaml")
        );
    }

    #[test]
    fn local_resolver_requires_a_principal_but_keeps_local_storage() {
        let err = LocalUserSpaceResolver.resolve("").unwrap_err();
        assert!(err.to_string().contains("principal id is required"));

        let resolved = LocalUserSpaceResolver
            .resolve("fp:test")
            .expect("local resolver should ignore principal storage partitioning");
        assert_eq!(resolved, UserSpacePaths::resolve().unwrap());
    }

    #[test]
    fn yaml_helpers_round_trip_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/config.yaml");

        let missing: Demo = read_yaml_or_default(&path).unwrap();
        assert_eq!(missing, Demo::default());

        write_yaml_atomic(&path, &Demo { value: "ok".into() }).unwrap();

        let loaded: Demo = read_yaml_or_default(&path).unwrap();
        assert_eq!(loaded.value, "ok");
        assert!(!path.with_extension("tmp~").exists());
    }

    #[cfg(unix)]
    #[test]
    fn yaml_helpers_write_private_files_and_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/config.yaml");

        write_yaml_atomic(&path, &Demo { value: "ok".into() }).unwrap();

        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn yaml_helpers_make_ai_dir_chain_private() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join(".ai")
            .join("state")
            .join("studio")
            .join("recent.yaml");

        write_yaml_atomic(&path, &Demo { value: "ok".into() }).unwrap();

        for dir in [".ai", ".ai/state", ".ai/state/studio"] {
            let mode = std::fs::metadata(tmp.path().join(dir))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "{dir} should be private");
        }
    }
}
