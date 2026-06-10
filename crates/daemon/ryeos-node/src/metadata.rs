use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonMetadata {
    pub pid: Option<u32>,
    pub bind: Option<String>,
    #[serde(default, alias = "socket")]
    pub uds_path: Option<PathBuf>,
    pub started_at: Option<String>,
    pub version: Option<String>,
    #[serde(default)]
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
        let mut metadata: DaemonMetadata = serde_json::from_str(&raw)
            .with_context(|| format!("parse daemon metadata at {}", path.display()))?;
        if metadata.app_root.as_os_str().is_empty() {
            metadata.app_root = app_root.to_path_buf();
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
