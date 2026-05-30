//! User-space path and YAML helpers for Studio/user-principal state.
//!
//! This module is the local-install seam for future principal/tenant-aware
//! resolution. Callers should use logical `config/*` and `state/*` paths here
//! instead of constructing `<user_root>/.ai/...` ad hoc.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSpacePaths {
    pub root: PathBuf,
}

impl UserSpacePaths {
    pub fn resolve() -> Result<Self> {
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
    let body = serde_yaml::to_string(value)
        .with_context(|| format!("failed to serialize {}", path.display()))?;
    crate::io::atomic::atomic_write(path, body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
