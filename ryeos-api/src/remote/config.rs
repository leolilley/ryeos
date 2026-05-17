//! Remote node configuration.
//!
//! Remotes are named connection targets stored in YAML. The config is
//! loaded from `<system_space_dir>/.ai/config/remotes/remotes.yaml`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Named remote configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteConfig {
    /// Human-readable name (also the key in the map).
    pub name: String,
    /// Base URL, e.g. `https://ryeos.example.com`.
    /// Must be HTTPS except for loopback addresses.
    pub url: String,
    /// Pinned principal_id of the remote node (from `/public-key`).
    pub principal_id: String,
    /// Remote node's vault X25519 public key (base64), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_public_key: Option<String>,
}

/// Full remotes file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemotesFile {
    remotes: HashMap<String, RemoteConfig>,
}

/// Path to the remotes config relative to system space dir.
const REMOTES_CONFIG_RELATIVE: &str = ".ai/config/remotes/remotes.yaml";

/// Load remotes config from disk. Returns empty map if file doesn't exist.
pub fn load_remotes(system_space_dir: &Path) -> Result<HashMap<String, RemoteConfig>> {
    let path = system_space_dir.join(REMOTES_CONFIG_RELATIVE);
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read remotes config: {}", path.display()))?;
    let file: RemotesFile = serde_yaml::from_str(&content)
        .with_context(|| format!("invalid remotes config: {}", path.display()))?;
    Ok(file.remotes)
}

/// Save remotes config to disk.
pub fn save_remotes(system_space_dir: &Path, remotes: &HashMap<String, RemoteConfig>) -> Result<()> {
    let path = system_space_dir.join(REMOTES_CONFIG_RELATIVE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = RemotesFile {
        remotes: remotes.clone(),
    };
    let content = serde_yaml::to_string(&file)?;
    std::fs::write(&path, content)
        .with_context(|| format!("failed to write remotes config: {}", path.display()))?;
    Ok(())
}

/// Get a named remote. Returns an error if not found.
pub fn get_remote(remotes: &HashMap<String, RemoteConfig>, name: &str) -> Result<RemoteConfig> {
    remotes
        .get(name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("remote '{}' not found in config", name))
}

/// Validate that a URL uses HTTPS or is a loopback address.
pub fn validate_url(url: &str) -> Result<()> {
    let parsed: url::Url = url.parse()
        .with_context(|| format!("invalid URL: {}", url))?;

    let scheme = parsed.scheme();
    if scheme != "https" {
        let host = parsed.host_str().unwrap_or("");
        let is_loopback = host == "localhost"
            || host == "127.0.0.1"
            || host == "::1"
            || host == "[::1]";
        if !is_loopback {
            anyhow::bail!(
                "remote URL must use HTTPS (got '{}'). Loopback addresses are allowed without TLS.",
                url
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_plain_http() {
        assert!(validate_url("http://example.com").is_err());
    }

    #[test]
    fn validate_accepts_https() {
        assert!(validate_url("https://example.com").is_ok());
    }

    #[test]
    fn validate_accepts_localhost_http() {
        assert!(validate_url("http://localhost:7400").is_ok());
        assert!(validate_url("http://127.0.0.1:7400").is_ok());
    }

    #[test]
    fn roundtrip() {
        let tmpdir = tempfile::tempdir().unwrap();
        let mut remotes = HashMap::new();
        remotes.insert("default".into(), RemoteConfig {
            name: "default".into(),
            url: "https://example.com".into(),
            principal_id: "fp:abc123".into(),
            vault_public_key: None,
        });
        save_remotes(tmpdir.path(), &remotes).unwrap();
        let loaded = load_remotes(tmpdir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["default"].url, "https://example.com");
    }
}
