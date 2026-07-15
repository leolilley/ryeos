use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DaemonMetadata {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub pid: Option<u32>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub bind: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub uds_path: Option<PathBuf>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub started_at: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub version: Option<String>,
    /// VCS revision the running daemon was built from. Recorded so `ryeos start`
    /// can detect an already-running daemon whose build differs from the
    /// on-disk binary (e.g. an install that replaced the binary but did not
    /// cycle the daemon). Null is explicit when build metadata is unavailable.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub revision: Option<String>,
    /// Build timestamp of the running daemon — a finer skew discriminator than
    /// `revision` (it differs across rebuilds at the same commit).
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub build_date: Option<String>,
    pub app_root: PathBuf,
}

impl DaemonMetadata {
    pub fn path(app_root: &Path) -> PathBuf {
        app_root.join("daemon.json")
    }

    pub fn read(app_root: &Path) -> Result<Option<Self>> {
        let path = Self::path(app_root);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
        };
        let metadata: DaemonMetadata = serde_json::from_str(&raw)
            .with_context(|| format!("parse daemon metadata at {}", path.display()))?;
        if metadata.app_root.as_os_str().is_empty() {
            anyhow::bail!("daemon metadata app_root must not be empty");
        }
        if metadata.app_root != app_root {
            anyhow::bail!(
                "daemon metadata app_root {} does not match {}",
                metadata.app_root.display(),
                app_root.display()
            );
        }
        Ok(Some(metadata))
    }

    pub fn write(&self, app_root: &Path) -> Result<()> {
        let path = Self::path(app_root);
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_metadata_has_one_exact_current_shape() {
        let complete = serde_json::json!({
            "pid": 42,
            "bind": "127.0.0.1:7400",
            "uds_path": "/tmp/ryeosd.sock",
            "started_at": "2026-07-15T00:00:00Z",
            "version": "test",
            "revision": null,
            "build_date": null,
            "app_root": "/tmp/ryeos",
        });
        assert!(serde_json::from_value::<DaemonMetadata>(complete.clone()).is_ok());

        for key in [
            "pid",
            "bind",
            "uds_path",
            "started_at",
            "version",
            "revision",
            "build_date",
            "app_root",
        ] {
            let mut missing = complete.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(
                serde_json::from_value::<DaemonMetadata>(missing).is_err(),
                "missing daemon metadata key {key} must fail exact decoding"
            );
        }

        let mut legacy_alias = complete;
        legacy_alias.as_object_mut().unwrap().remove("uds_path");
        legacy_alias["socket"] = serde_json::json!("/tmp/legacy.sock");
        assert!(serde_json::from_value::<DaemonMetadata>(legacy_alias).is_err());
    }
}
