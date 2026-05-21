//! Remote node configuration.
//!
//! Remotes are named connection targets stored in YAML. The config is
//! loaded from `<system_space_dir>/.ai/config/remotes/remotes.yaml`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub use ryeos_state::project_sync::ProjectSyncScope;

/// Explicit local-to-remote project binding for a configured remote.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteProjectBinding {
    /// Absolute project path on the remote node. This is not
    /// canonicalized locally; the remote canonicalizes it before use.
    pub remote_project_path: String,
    /// Sync/execution scope for this binding.
    #[serde(default)]
    pub sync_scope: ProjectSyncScope,
}

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
    /// Remote node's vault X25519 public key fingerprint.
    /// Required — populated during `remote configure`.
    pub vault_fingerprint: String,
    /// Cached remote ingest-ignore config, populated during
    /// `remote configure`. Required for push to use the correct
    /// ignore rules. Re-run `remote configure` if stale.
    pub ingest_ignore: ryeos_app::ignore::IgnoreConfig,
    /// Canonical local project path -> remote project binding.
    #[serde(default)]
    pub project_bindings: HashMap<String, RemoteProjectBinding>,
}

/// Resolved local/remote project path binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProjectBinding {
    pub local_project_path: PathBuf,
    pub remote_project_path: String,
    pub sync_scope: ProjectSyncScope,
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
pub fn save_remotes(
    system_space_dir: &Path,
    remotes: &HashMap<String, RemoteConfig>,
) -> Result<()> {
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

/// Canonicalize a local project path for config lookup.
pub fn canonical_local_project_path(project_path: &Path) -> Result<PathBuf> {
    if !project_path.is_absolute() {
        anyhow::bail!(
            "project '{}' is not absolute: pass an absolute project path",
            project_path.display()
        );
    }
    project_path.canonicalize().with_context(|| {
        format!(
            "cannot canonicalize project '{}'; ensure it exists",
            project_path.display()
        )
    })
}

/// Resolve a configured local->remote project binding.
pub fn resolve_project_binding(
    remote: &RemoteConfig,
    local_project_path: &Path,
) -> Result<ResolvedProjectBinding> {
    let canonical = canonical_local_project_path(local_project_path)?;
    let key = canonical.to_string_lossy().to_string();
    let binding = remote.project_bindings.get(&key).ok_or_else(|| {
        anyhow::anyhow!(
            "remote '{}' has no project binding for '{}'; run `ryeos remote bind-project --remote {} --project {} --remote-project <remote-path> --sync-scope ai_only`",
            remote.name,
            key,
            remote.name,
            key,
        )
    })?;
    validate_remote_project_path(&binding.remote_project_path)?;

    Ok(ResolvedProjectBinding {
        local_project_path: canonical,
        remote_project_path: binding.remote_project_path.clone(),
        sync_scope: binding.sync_scope,
    })
}

/// Validate the remote path locally without canonicalizing it here.
pub fn validate_remote_project_path(remote_project_path: &str) -> Result<()> {
    if remote_project_path.is_empty() {
        anyhow::bail!("remote_project_path must not be empty");
    }
    if !Path::new(remote_project_path).is_absolute() {
        anyhow::bail!(
            "remote_project_path '{}' is not absolute; remote project paths must be absolute on the remote node",
            remote_project_path
        );
    }
    Ok(())
}

/// Validate that a URL uses HTTPS or is a loopback address.
pub fn validate_url(url: &str) -> Result<()> {
    let parsed: url::Url = url
        .parse()
        .with_context(|| format!("invalid URL: {}", url))?;

    let scheme = parsed.scheme();
    if scheme != "https" {
        let host = parsed.host_str().unwrap_or("");
        let is_loopback =
            host == "localhost" || host == "127.0.0.1" || host == "::1" || host == "[::1]";
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
        remotes.insert(
            "default".into(),
            RemoteConfig {
                name: "default".into(),
                url: "https://example.com".into(),
                principal_id: "fp:abc123".into(),
                vault_fingerprint: "sha256:def456".into(),
                ingest_ignore: ryeos_app::ignore::IgnoreConfig {
                    patterns: vec![".git/".into(), "target/".into()],
                },
                project_bindings: HashMap::new(),
            },
        );
        save_remotes(tmpdir.path(), &remotes).unwrap();
        let loaded = load_remotes(tmpdir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["default"].url, "https://example.com");
        assert_eq!(loaded["default"].vault_fingerprint, "sha256:def456");
        assert_eq!(loaded["default"].ingest_ignore.patterns.len(), 2);
    }

    #[test]
    fn config_without_required_fields_fails_to_parse() {
        // A remotes.yaml missing vault_fingerprint or ingest_ignore
        // should fail to parse — no tolerance for incomplete configs.
        let tmpdir = tempfile::tempdir().unwrap();
        let path = tmpdir.path().join(".ai/config/remotes/remotes.yaml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"
remotes:
  bare:
    name: bare
    url: https://example.com
    principal_id: fp:abc
"#,
        )
        .unwrap();
        let result = load_remotes(tmpdir.path());
        assert!(
            result.is_err(),
            "should fail on missing required fields, got {:?}",
            result
        );
    }

    #[test]
    fn roundtrip_with_project_bindings() {
        let tmpdir = tempfile::tempdir().unwrap();
        let local = tmpdir.path().join("project");
        std::fs::create_dir_all(&local).unwrap();
        let local_key = local.canonicalize().unwrap().to_string_lossy().to_string();

        let mut bindings = HashMap::new();
        bindings.insert(
            local_key.clone(),
            RemoteProjectBinding {
                remote_project_path: "/data/projects/example".into(),
                sync_scope: ProjectSyncScope::AiOnly,
            },
        );

        let mut remotes = HashMap::new();
        remotes.insert(
            "railway".into(),
            RemoteConfig {
                name: "railway".into(),
                url: "https://example.com".into(),
                principal_id: "fp:abc123".into(),
                vault_fingerprint: "sha256:def456".into(),
                ingest_ignore: ryeos_app::ignore::IgnoreConfig {
                    patterns: vec![".git/".into()],
                },
                project_bindings: bindings,
            },
        );

        save_remotes(tmpdir.path(), &remotes).unwrap();
        let loaded = load_remotes(tmpdir.path()).unwrap();
        let resolved = resolve_project_binding(&loaded["railway"], &local).unwrap();
        assert_eq!(resolved.remote_project_path, "/data/projects/example");
        assert_eq!(resolved.sync_scope, ProjectSyncScope::AiOnly);
    }
}
