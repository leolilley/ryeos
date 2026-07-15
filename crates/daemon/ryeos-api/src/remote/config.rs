//! Remote node configuration.
//!
//! Remotes are named connection targets stored in YAML. The config is
//! loaded from `<app_root>/.ai/config/remotes/remotes.yaml`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine as _;
use serde::de::Error as _;
use serde::{Deserialize, Serialize};

pub use ryeos_state::project_sync::ProjectSyncScope;

/// Provider/import descriptor for a remote RyeOS node.
///
/// A descriptor is a trust pin and discovery convenience, not a credential.
/// Importing one records the expected node identity for `remote configure`,
/// which still verifies the live node before writing local remote config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RemoteDescriptor {
    pub version: u32,
    pub name: Option<String>,
    pub url: String,
    pub node: RemoteDescriptorNode,
    #[serde(default)]
    pub capabilities: Option<serde_yaml::Value>,
    #[serde(default)]
    pub admission: Option<serde_yaml::Value>,
    #[serde(default)]
    pub provider: Option<serde_yaml::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteDescriptorNode {
    /// Remote node Ed25519 public key in `ed25519:<base64>` form.
    pub public_key: String,
    /// Expected fingerprint for `public_key`.
    pub fingerprint: String,
}

impl RemoteDescriptor {
    pub fn from_yaml_str(input: &str) -> Result<Self> {
        let descriptor: Self = serde_yaml::from_str(input).context("invalid remote descriptor")?;
        descriptor.validate()?;
        Ok(descriptor)
    }

    pub fn from_path(path: &Path) -> Result<Self> {
        let input = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read remote descriptor: {}", path.display()))?;
        Self::from_yaml_str(&input)
            .with_context(|| format!("invalid remote descriptor: {}", path.display()))
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            anyhow::bail!(
                "unsupported remote descriptor version {}; expected 1",
                self.version
            );
        }
        if let Some(name) = &self.name {
            if name.trim().is_empty() {
                anyhow::bail!("remote descriptor name must not be empty when present");
            }
        }
        validate_url(&self.url)?;
        let key = decode_signing_key(&self.node.public_key)
            .context("invalid remote descriptor node.public_key")?;
        let actual_fingerprint = lillux::crypto::fingerprint(&key);
        if self.node.fingerprint != actual_fingerprint {
            anyhow::bail!(
                "remote descriptor fingerprint mismatch: node.fingerprint {}, public_key fingerprint {}",
                self.node.fingerprint,
                actual_fingerprint,
            );
        }
        Ok(())
    }
}

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
///
/// Authored by `ryeos remote configure`, which contacts the remote's
/// `/public-key` endpoint and populates every field. Hand-edited
/// stubs that omit fields will be rejected at load time with a
/// per-entry warning (the rest of the file still loads).
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
    /// Pinned remote daemon Ed25519 verifying key in `ed25519:<base64>` form.
    /// This is the trust anchor for verified federation/import surfaces.
    pub signing_key: String,
    /// Remote node's daemon site identity (e.g. `"site:gpu-node"`).
    /// Discovered from the remote's `/public-key` response during
    /// `remote configure`. Used by target-site forwarding to resolve
    /// `target_site_id` to a remote connection.
    pub site_id: String,
    /// Remote node's vault X25519 public key fingerprint.
    /// Required — populated during `remote configure`.
    pub vault_fingerprint: String,
    /// Cached remote ingest-ignore config, populated during
    /// `remote configure`. Required for push to use the correct
    /// ignore rules. Re-run `remote configure` if stale.
    pub ingest_ignore: ryeos_app::ignore::IgnoreConfig,
    /// Canonical local project path -> remote project binding.
    ///
    /// These bindings are operator-local and are written only to the
    /// user/operator remotes file. Project-level remotes may be layered for
    /// read-only remote definition, but project-to-remote bindings still
    /// live here.
    #[serde(default)]
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub project_bindings: HashMap<String, RemoteProjectBinding>,
}

impl RemoteConfig {
    /// Decode and validate the pinned daemon signing key against the
    /// remote's pinned principal/fingerprint identity.
    pub fn pinned_signing_key(&self) -> Result<lillux::crypto::VerifyingKey> {
        let key = decode_signing_key(&self.signing_key)?;
        let fingerprint = lillux::crypto::fingerprint(&key);
        let expected_principal = format!("fp:{fingerprint}");
        if self.principal_id != expected_principal {
            anyhow::bail!(
                "remote '{}' principal_id '{}' does not match signing_key fingerprint '{}'",
                self.name,
                self.principal_id,
                expected_principal,
            );
        }
        Ok(key)
    }

    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            anyhow::bail!("remote name must not be empty");
        }
        validate_url(&self.url)?;
        self.pinned_signing_key()?;
        if self.site_id.trim().is_empty() {
            anyhow::bail!("remote '{}' site_id must not be empty", self.name);
        }
        if self.vault_fingerprint.trim().is_empty() {
            anyhow::bail!("remote '{}' vault_fingerprint must not be empty", self.name);
        }
        for (local_project_path, binding) in &self.project_bindings {
            let canonical = canonical_local_project_path(Path::new(local_project_path))
                .with_context(|| {
                    format!(
                        "remote '{}' has invalid local project binding key '{}'",
                        self.name, local_project_path
                    )
                })?;
            let canonical_identity = local_project_identity(&canonical)?;
            if canonical_identity != local_project_path {
                anyhow::bail!(
                    "remote '{}' local project binding key '{}' is not canonical; expected '{}'",
                    self.name,
                    local_project_path,
                    canonical_identity
                );
            }
            validate_remote_project_path(&binding.remote_project_path)?;
        }
        Ok(())
    }
}

/// Where a loaded remote config came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteConfigScope {
    /// Operator-level runtime config under the app root.
    Operator,
    /// Project-level config rooted at this local project path.
    Project { root: PathBuf },
}

impl RemoteConfigScope {
    pub fn label(&self) -> &'static str {
        match self {
            RemoteConfigScope::Operator => "operator",
            RemoteConfigScope::Project { .. } => "project",
        }
    }
}

/// A valid remote config plus its source scope.
#[derive(Debug, Clone)]
pub struct LoadedRemote {
    pub config: RemoteConfig,
    pub scope: RemoteConfigScope,
    pub config_path: PathBuf,
}

/// A remote entry that was present on disk but failed strict validation.
#[derive(Debug, Clone)]
pub struct InvalidRemote {
    pub name: String,
    pub scope: RemoteConfigScope,
    pub config_path: PathBuf,
    pub error: String,
    pub url: Option<String>,
    pub repair_hint: String,
}

/// Layered remote load result with both usable configs and diagnostics.
#[derive(Debug, Clone, Default)]
pub struct RemotesLoadReport {
    pub remotes: HashMap<String, LoadedRemote>,
    pub invalid: Vec<InvalidRemote>,
}

/// Resolved local/remote project path binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProjectBinding {
    pub local_project_path: PathBuf,
    pub remote_project_path: String,
    pub sync_scope: ProjectSyncScope,
}

/// Full remotes file. Top-level structure tolerates extra fields so
/// older or richer (e.g. project-level) configs keep loading; only the
/// `remotes` map is consumed here.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemotesFile {
    #[serde(default)]
    remotes: HashMap<String, serde_yaml::Value>,
}

/// Path under [`ryeos_engine::AI_DIR`] where the remotes config lives.
pub const REMOTES_CONFIG_SUBPATH: &[&str] = &["config", "remotes", "remotes.yaml"];

/// Resolve the absolute path to a remotes config file given a
/// space/project root.
pub fn remotes_config_path(root: &Path) -> PathBuf {
    let mut p = root.join(ryeos_engine::AI_DIR);
    for seg in REMOTES_CONFIG_SUBPATH {
        p = p.join(seg);
    }
    p
}

/// Load remotes config from disk. Returns empty map if file doesn't exist.
///
/// Resilience: a single malformed entry (e.g. missing required
/// `site_id` from an older config) is skipped with a warning to
/// stderr rather than failing the whole file load. Operators are
/// guided to repair such entries via `ryeos remote configure`, but
/// stale entries must not block listing or using other valid remotes,
/// including project-level overrides layered on top.
pub fn load_remotes(app_root: &Path) -> Result<HashMap<String, RemoteConfig>> {
    let path = remotes_config_path(app_root);
    load_remotes_at(&path)
}

/// Load a remotes file from an explicit absolute path. Same per-entry
/// resilience as [`load_remotes`].
pub fn load_remotes_at(path: &Path) -> Result<HashMap<String, RemoteConfig>> {
    Ok(load_remotes_at_report(path, RemoteConfigScope::Operator)?
        .remotes
        .into_iter()
        .map(|(name, loaded)| (name, loaded.config))
        .collect())
}

fn load_remotes_at_report(path: &Path, scope: RemoteConfigScope) -> Result<RemotesLoadReport> {
    let mut report = RemotesLoadReport::default();
    if !path.exists() {
        return Ok(report);
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read remotes config: {}", path.display()))?;
    let file: RemotesFile = serde_yaml::from_str(&content)
        .with_context(|| format!("invalid remotes config: {}", path.display()))?;
    for (name, raw) in file.remotes {
        let url = raw
            .get("url")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned);
        match serde_yaml::from_value::<RemoteConfig>(raw).and_then(|cfg| {
            if cfg.name != name {
                return Err(serde_yaml::Error::custom(format!(
                    "remote map key '{}' does not match remote.name '{}'",
                    name, cfg.name,
                )));
            }
            cfg.validate()
                .map_err(|e| serde_yaml::Error::custom(e.to_string()))?;
            if matches!(scope, RemoteConfigScope::Project { .. }) && !cfg.project_bindings.is_empty()
            {
                return Err(serde_yaml::Error::custom(
                    "project remotes must not contain project_bindings; run `ryeos remote bind-project` to create operator-local bindings",
                ));
            }
            Ok(cfg)
        }) {
            Ok(cfg) => {
                report.remotes.insert(
                    name,
                    LoadedRemote {
                        config: cfg,
                        scope: scope.clone(),
                        config_path: path.to_path_buf(),
                    },
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "ryeos_api::remote::config",
                    config_path = %path.display(),
                    remote = %name,
                    error = %e,
                    "skipping malformed remote entry; run `ryeos remote configure --remote {}` to repair or remove the entry from {}",
                    name,
                    path.display(),
                );
                let repair_hint = match &scope {
                    RemoteConfigScope::Project { .. } => format!(
                        "edit or remove {} and re-sign the project config",
                        path.display()
                    ),
                    RemoteConfigScope::Operator => match url.as_deref() {
                        Some(url) => format!("run `ryeos remote configure {} --url {}`", name, url),
                        None => format!("run `ryeos remote configure {} --url <https-url>`", name),
                    },
                };
                report.invalid.push(InvalidRemote {
                    name,
                    scope: scope.clone(),
                    config_path: path.to_path_buf(),
                    error: e.to_string(),
                    url,
                    repair_hint,
                });
            }
        }
    }
    Ok(report)
}

/// Load remotes layered project-over-user.
///
/// Loads the operator-level remotes file from `app_root`, then if
/// `project_path` is provided, loads project-level remotes from
/// `<project>/.ai/config/remotes/remotes.yaml` and merges them on top
/// (project entries win on name collision). Either side missing or
/// individually malformed does not prevent the other from being used.
pub fn load_remotes_layered(
    app_root: &Path,
    project_path: Option<&Path>,
) -> Result<HashMap<String, RemoteConfig>> {
    Ok(load_remotes_layered_report(app_root, project_path)?
        .remotes
        .into_iter()
        .map(|(name, loaded)| (name, loaded.config))
        .collect())
}

/// Load operator remotes plus optional project remotes, retaining invalid
/// entries as diagnostics for user-facing commands.
pub fn load_remotes_layered_report(
    app_root: &Path,
    project_path: Option<&Path>,
) -> Result<RemotesLoadReport> {
    let mut report =
        load_remotes_at_report(&remotes_config_path(app_root), RemoteConfigScope::Operator)?;
    let user_bindings: HashMap<String, HashMap<String, RemoteProjectBinding>> = report
        .remotes
        .iter()
        .map(|(name, loaded)| (name.clone(), loaded.config.project_bindings.clone()))
        .collect();
    if let Some(project) = project_path {
        let canonical_project = canonical_local_project_path(project)?;
        let project_report = load_remotes_at_report(
            &remotes_config_path(&canonical_project),
            RemoteConfigScope::Project {
                root: canonical_project,
            },
        )?;
        for (name, mut loaded) in project_report.remotes {
            if let Some(bindings) = user_bindings.get(&name) {
                loaded.config.project_bindings = bindings.clone();
            }
            report.remotes.insert(name, loaded);
        }
        report.invalid.extend(project_report.invalid);
    }
    Ok(report)
}

/// Save remotes config to disk.
pub fn save_remotes(app_root: &Path, remotes: &HashMap<String, RemoteConfig>) -> Result<()> {
    for (name, cfg) in remotes {
        if name.trim().is_empty() {
            anyhow::bail!("remote config key must not be empty");
        }
        if cfg.name != *name {
            anyhow::bail!(
                "remote map key '{}' does not match remote.name '{}'",
                name,
                cfg.name,
            );
        }
        cfg.validate()
            .with_context(|| format!("invalid remote '{}'", name))?;
    }
    let path = remotes_config_path(app_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_yaml::to_string(&serde_json::json!({ "remotes": remotes }))?;
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

pub fn get_loaded_remote(
    remotes: &HashMap<String, LoadedRemote>,
    name: &str,
) -> Result<LoadedRemote> {
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

/// Exact UTF-8 representation used as the project-binding identity. Paths
/// that cannot be represented losslessly are not valid binding authorities.
pub fn local_project_identity(project_path: &Path) -> Result<&str> {
    project_path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "canonical local project path '{}' is not valid UTF-8",
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
    let key = local_project_identity(&canonical)?;
    let binding = remote.project_bindings.get(key).ok_or_else(|| {
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

/// Resolve a local->remote binding from operator-local project_bindings.
pub fn resolve_loaded_project_binding(
    loaded: &LoadedRemote,
    local_project_path: &Path,
) -> Result<ResolvedProjectBinding> {
    resolve_project_binding(&loaded.config, local_project_path)
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

    let host = parsed.host_str().unwrap_or("");
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let is_loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
    match parsed.scheme() {
        "https" => {}
        "http" if is_loopback => {}
        other => {
            anyhow::bail!(
                "remote URL must use HTTPS except HTTP loopback (got scheme '{}' in '{}')",
                other,
                url
            );
        }
    }
    Ok(())
}

pub fn decode_signing_key(input: &str) -> Result<lillux::crypto::VerifyingKey> {
    let b64 = input
        .strip_prefix("ed25519:")
        .ok_or_else(|| anyhow::anyhow!("signing_key must start with ed25519:"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("failed to decode signing_key")?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("signing_key must contain 32 raw Ed25519 public key bytes"))?;
    Ok(lillux::crypto::VerifyingKey::from_bytes(&bytes)?)
}

#[cfg(test)]
fn test_signing_key(seed: u8) -> String {
    let signing_key = lillux::crypto::SigningKey::from_bytes(&[seed; 32]);
    format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD.encode(signing_key.verifying_key().as_bytes())
    )
}

#[cfg(test)]
fn test_principal_id(seed: u8) -> String {
    let key = decode_signing_key(&test_signing_key(seed)).unwrap();
    format!("fp:{}", lillux::crypto::fingerprint(&key))
}

#[cfg(test)]
fn test_fingerprint(seed: u8) -> String {
    let key = decode_signing_key(&test_signing_key(seed)).unwrap();
    lillux::crypto::fingerprint(&key)
}

// ── Target-site forwarding lookup ────────────────────────────────────

/// A remote resolved from a `target_site_id` lookup.
#[derive(Debug, Clone)]
pub struct ResolvedRemote {
    /// The matching remote config.
    pub remote: RemoteConfig,
    /// The key (name) under which the remote is stored.
    pub config_key: String,
}

/// Errors from target-site resolution.
#[derive(Debug, thiserror::Error)]
pub enum TargetSiteError {
    /// No configured remote has this site_id.
    #[error("unknown target site '{target_site_id}'; configured sites: [{known_sites}]")]
    UnknownSite {
        target_site_id: String,
        known_sites: String,
    },
    /// Multiple remotes share the same site_id — ambiguous.
    #[error(
        "ambiguous target site '{target_site_id}': remotes [{remotes}] share this site_id; \
         reconfigure to use unique site IDs"
    )]
    AmbiguousSite {
        target_site_id: String,
        remotes: String,
    },
}

/// Resolve a configured remote by its `site_id`.
///
/// Looks up all configured remotes and finds the one whose `site_id`
/// matches `target_site_id`. Returns an error if:
/// - no remote has the target site_id
/// - multiple remotes share the same site_id (ambiguous)
pub fn resolve_remote_by_site_id(
    remotes: &HashMap<String, RemoteConfig>,
    target_site_id: &str,
) -> Result<ResolvedRemote, TargetSiteError> {
    let mut matches: Vec<(&String, &RemoteConfig)> = Vec::new();
    let mut all_site_ids: Vec<String> = Vec::new();

    for (key, remote) in remotes {
        all_site_ids.push(remote.site_id.clone());

        if remote.site_id == target_site_id {
            matches.push((key, remote));
        }
    }

    if matches.is_empty() {
        all_site_ids.sort();
        all_site_ids.dedup();
        return Err(TargetSiteError::UnknownSite {
            target_site_id: target_site_id.to_string(),
            known_sites: all_site_ids.join(", "),
        });
    }

    if matches.len() > 1 {
        let names: Vec<&str> = matches.iter().map(|(k, _)| k.as_str()).collect();
        return Err(TargetSiteError::AmbiguousSite {
            target_site_id: target_site_id.to_string(),
            remotes: names.join(", "),
        });
    }

    let (config_key, remote) = matches.into_iter().next().unwrap();
    Ok(ResolvedRemote {
        remote: remote.clone(),
        config_key: config_key.clone(),
    })
}

pub fn resolve_loaded_remote_by_site_id(
    remotes: &HashMap<String, LoadedRemote>,
    target_site_id: &str,
) -> Result<LoadedRemote, TargetSiteError> {
    let plain: HashMap<String, RemoteConfig> = remotes
        .iter()
        .map(|(name, loaded)| (name.clone(), loaded.config.clone()))
        .collect();
    let resolved = resolve_remote_by_site_id(&plain, target_site_id)?;
    Ok(remotes
        .get(&resolved.config_key)
        .cloned()
        .expect("resolved config key must exist in source map"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_descriptor_validates_key_fingerprint_binding() {
        let descriptor = RemoteDescriptor::from_yaml_str(&format!(
            r#"
version: 1
name: hosted-prod
url: https://node.example.com
node:
  public_key: {}
  fingerprint: {}
capabilities:
  remote_execute: true
admission:
  methods: [one_time_code]
provider:
  name: RyeOS Cloud
"#,
            test_signing_key(7),
            test_fingerprint(7),
        ))
        .unwrap();

        assert_eq!(descriptor.name.as_deref(), Some("hosted-prod"));
        assert_eq!(descriptor.url, "https://node.example.com");
    }

    #[test]
    fn remote_descriptor_rejects_mismatched_fingerprint() {
        let err = RemoteDescriptor::from_yaml_str(&format!(
            r#"
version: 1
url: https://node.example.com
node:
  public_key: {}
  fingerprint: {}
"#,
            test_signing_key(7),
            test_fingerprint(8),
        ))
        .unwrap_err();

        assert!(
            err.to_string().contains("fingerprint mismatch"),
            "unexpected error: {err:#}"
        );
    }

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
        assert!(validate_url("http://127.0.0.2:7400").is_ok());
        assert!(validate_url("http://[::1]:7400").is_ok());
    }

    #[test]
    fn validate_rejects_loopback_looking_hostname() {
        assert!(validate_url("http://127.example.com").is_err());
    }

    #[test]
    fn validate_rejects_non_http_loopback_scheme() {
        assert!(validate_url("ftp://localhost:7400").is_err());
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
                principal_id: test_principal_id(1),
                signing_key: test_signing_key(1),
                site_id: "site:example".into(),
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
        assert_eq!(loaded["default"].site_id, "site:example");
        assert_eq!(loaded["default"].vault_fingerprint, "sha256:def456");
        assert_eq!(loaded["default"].ingest_ignore.patterns.len(), 2);
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
            "example-remote".into(),
            RemoteConfig {
                name: "example-remote".into(),
                url: "https://example.com".into(),
                principal_id: test_principal_id(2),
                signing_key: test_signing_key(2),
                site_id: "site:example".into(),
                vault_fingerprint: "sha256:def456".into(),
                ingest_ignore: ryeos_app::ignore::IgnoreConfig {
                    patterns: vec![".git/".into()],
                },
                project_bindings: bindings,
            },
        );

        save_remotes(tmpdir.path(), &remotes).unwrap();
        let loaded = load_remotes(tmpdir.path()).unwrap();
        let resolved = resolve_project_binding(&loaded["example-remote"], &local).unwrap();
        assert_eq!(resolved.remote_project_path, "/data/projects/example");
        assert_eq!(resolved.sync_scope, ProjectSyncScope::AiOnly);
    }

    #[test]
    fn load_remotes_layered_accepts_project_remotes_with_category() {
        let app_root = tempfile::tempdir().unwrap();
        let project_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project_root.path().join(ryeos_engine::AI_DIR)).unwrap();
        let project_path = remotes_config_path(project_root.path());
        std::fs::create_dir_all(project_path.parent().unwrap()).unwrap();
        std::fs::write(
            &project_path,
            format!(
                r#"
category: remotes
remotes:
  example:
    name: example
    url: https://project-level.example.com
    principal_id: {}
    signing_key: {}
    site_id: site:example
    vault_fingerprint: sha256:example
    ingest_ignore:
      patterns: []
"#,
                test_principal_id(2),
                test_signing_key(2),
            ),
        )
        .unwrap();

        let report =
            load_remotes_layered_report(app_root.path(), Some(project_root.path())).unwrap();
        assert_eq!(
            report.remotes["example"].config.url,
            "https://project-level.example.com"
        );
        assert!(matches!(
            report.remotes["example"].scope,
            RemoteConfigScope::Project { .. }
        ));
    }

    #[test]
    fn load_remotes_layered_preserves_operator_bindings_on_project_override() {
        let app_root = tempfile::tempdir().unwrap();
        let project_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project_root.path()).unwrap();
        let project_root_path = project_root.path().canonicalize().unwrap();
        let local_key = project_root_path.to_string_lossy().to_string();

        let mut bindings = HashMap::new();
        bindings.insert(
            local_key.clone(),
            RemoteProjectBinding {
                remote_project_path: "/remote/project".into(),
                sync_scope: ProjectSyncScope::FullProject,
            },
        );
        let mut operator_remotes = HashMap::new();
        operator_remotes.insert(
            "example".into(),
            RemoteConfig {
                name: "example".into(),
                url: "https://user.example.com".into(),
                principal_id: test_principal_id(3),
                signing_key: test_signing_key(3),
                site_id: "site:user".into(),
                vault_fingerprint: "sha256:user".into(),
                ingest_ignore: ryeos_app::ignore::IgnoreConfig { patterns: vec![] },
                project_bindings: bindings,
            },
        );
        save_remotes(app_root.path(), &operator_remotes).unwrap();

        let project_path = remotes_config_path(&project_root_path);
        std::fs::create_dir_all(project_path.parent().unwrap()).unwrap();
        std::fs::write(
            &project_path,
            format!(
                r#"
remotes:
  example:
    name: example
    url: https://project.example.com
    principal_id: {}
    signing_key: {}
    site_id: site:project
    vault_fingerprint: sha256:project
    ingest_ignore:
      patterns: []
"#,
                test_principal_id(4),
                test_signing_key(4),
            ),
        )
        .unwrap();

        let report =
            load_remotes_layered_report(app_root.path(), Some(&project_root_path)).unwrap();
        let loaded = &report.remotes["example"];
        assert_eq!(loaded.config.url, "https://project.example.com");
        let binding = resolve_loaded_project_binding(loaded, &project_root_path).unwrap();
        assert_eq!(binding.remote_project_path, "/remote/project");
        assert_eq!(binding.sync_scope, ProjectSyncScope::FullProject);
    }

    #[test]
    fn load_remotes_layered_rejects_project_sourced_bindings() {
        let app_root = tempfile::tempdir().unwrap();
        let project_root = tempfile::tempdir().unwrap();
        let project_path = remotes_config_path(project_root.path());
        std::fs::create_dir_all(project_path.parent().unwrap()).unwrap();
        std::fs::write(
            &project_path,
            format!(
                r#"
remotes:
  example:
    name: example
    url: https://project.example.com
    principal_id: {}
    signing_key: {}
    site_id: site:project
    vault_fingerprint: sha256:project
    ingest_ignore:
      patterns: []
    project_bindings:
      /tmp/local:
        remote_project_path: /remote/project
        sync_scope: full_project
"#,
                test_principal_id(5),
                test_signing_key(5),
            ),
        )
        .unwrap();

        let report =
            load_remotes_layered_report(app_root.path(), Some(project_root.path())).unwrap();
        assert!(!report.remotes.contains_key("example"));
        assert_eq!(report.invalid.len(), 1);
        assert!(report.invalid[0].error.contains("project_bindings"));
    }

    #[test]
    fn save_remotes_rejects_invalid_map_key() {
        let tmpdir = tempfile::tempdir().unwrap();
        let mut remotes = HashMap::new();
        remotes.insert(
            "".into(),
            RemoteConfig {
                name: "example".into(),
                url: "https://example.com".into(),
                principal_id: test_principal_id(6),
                signing_key: test_signing_key(6),
                site_id: "site:example".into(),
                vault_fingerprint: "sha256:example".into(),
                ingest_ignore: ryeos_app::ignore::IgnoreConfig { patterns: vec![] },
                project_bindings: HashMap::new(),
            },
        );
        let err = save_remotes(tmpdir.path(), &remotes).unwrap_err();
        assert!(err.to_string().contains("key must not be empty"));
    }

    #[test]
    fn layered_report_includes_invalid_missing_signing_key() {
        let app_root = tempfile::tempdir().unwrap();
        let project_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project_root.path().join(ryeos_engine::AI_DIR)).unwrap();
        let project_path = remotes_config_path(project_root.path());
        std::fs::create_dir_all(project_path.parent().unwrap()).unwrap();
        std::fs::write(
            &project_path,
            r#"
remotes:
  default:
    name: default
    url: https://project-level.example.com
    principal_id: fp:stale
    site_id: site:stale
    vault_fingerprint: sha256:stale
    ingest_ignore:
      patterns: []
"#,
        )
        .unwrap();

        let report =
            load_remotes_layered_report(app_root.path(), Some(project_root.path())).unwrap();
        assert!(!report.remotes.contains_key("default"));
        assert_eq!(report.invalid.len(), 1);
        assert_eq!(report.invalid[0].name, "default");
        assert!(report.invalid[0].error.contains("signing_key"));
        assert!(report.invalid[0].repair_hint.contains("re-sign"));
    }

    // ── resolve_remote_by_site_id tests ─────────────────────────

    fn make_remote(name: &str, site_id: &str) -> RemoteConfig {
        let seed = name.as_bytes().first().copied().unwrap_or(1);
        RemoteConfig {
            name: name.to_string(),
            url: format!("https://{}.example.com", name),
            principal_id: test_principal_id(seed),
            signing_key: test_signing_key(seed),
            site_id: site_id.to_string(),
            vault_fingerprint: "sha256:test".to_string(),
            ingest_ignore: ryeos_app::ignore::IgnoreConfig { patterns: vec![] },
            project_bindings: HashMap::new(),
        }
    }

    #[test]
    fn lookup_by_site_id_succeeds() {
        let mut remotes = HashMap::new();
        remotes.insert("gpu".into(), make_remote("gpu", "site:gpu-node"));
        let result = resolve_remote_by_site_id(&remotes, "site:gpu-node").unwrap();
        assert_eq!(result.config_key, "gpu");
        assert_eq!(result.remote.site_id, "site:gpu-node");
    }

    #[test]
    fn lookup_unknown_site_id_returns_error_with_known_sites() {
        let mut remotes = HashMap::new();
        remotes.insert("gpu".into(), make_remote("gpu", "site:gpu-node"));
        let err = resolve_remote_by_site_id(&remotes, "site:unknown").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("site:unknown"),
            "should name target, got: {msg}"
        );
        assert!(
            msg.contains("site:gpu-node"),
            "should list known sites, got: {msg}"
        );
        assert!(
            matches!(err, TargetSiteError::UnknownSite { .. }),
            "must be UnknownSite variant"
        );
    }

    #[test]
    fn lookup_ambiguous_site_id_returns_error() {
        let mut remotes = HashMap::new();
        remotes.insert("gpu1".into(), make_remote("gpu1", "site:gpu"));
        remotes.insert("gpu2".into(), make_remote("gpu2", "site:gpu"));
        let err = resolve_remote_by_site_id(&remotes, "site:gpu").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("ambiguous"),
            "should say ambiguous, got: {msg}"
        );
        assert!(msg.contains("gpu1"), "should name both remotes, got: {msg}");
        assert!(msg.contains("gpu2"), "should name both remotes, got: {msg}");
        assert!(
            matches!(err, TargetSiteError::AmbiguousSite { .. }),
            "must be AmbiguousSite variant"
        );
    }

    #[test]
    fn lookup_empty_remotes_returns_unknown() {
        let remotes = HashMap::new();
        let err = resolve_remote_by_site_id(&remotes, "site:anything").unwrap_err();
        assert!(
            matches!(err, TargetSiteError::UnknownSite { ref known_sites, .. } if known_sites.is_empty()),
            "no remotes → empty known_sites, got: {err:?}"
        );
    }

    #[test]
    fn config_without_site_id_is_skipped_not_fatal() {
        let tmpdir = tempfile::tempdir().unwrap();
        let path = remotes_config_path(tmpdir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                r#"
remotes:
  old_remote:
    name: old_remote
    url: https://example.com
    principal_id: fp:old
    vault_fingerprint: sha256:old
    ingest_ignore:
      patterns: []
  good:
    name: good
    url: https://good.example.com
    principal_id: {}
    signing_key: {}
    site_id: site:good
    vault_fingerprint: sha256:good
    ingest_ignore:
      patterns: []
"#,
                test_principal_id(4),
                test_signing_key(4),
            ),
        )
        .unwrap();
        let loaded = load_remotes(tmpdir.path()).expect(
            "load_remotes must not fail when one entry is malformed: \
             stale entries must not block listing other valid remotes",
        );
        assert!(
            !loaded.contains_key("old_remote"),
            "malformed entry must be skipped, got: {loaded:?}"
        );
        assert!(
            loaded.contains_key("good"),
            "valid entry must remain loadable, got: {loaded:?}"
        );
    }

    #[test]
    fn load_remotes_layered_merges_project_over_operator() {
        let app_root = tempfile::tempdir().unwrap();
        let project_root = tempfile::tempdir().unwrap();

        let operator_path = remotes_config_path(app_root.path());
        std::fs::create_dir_all(operator_path.parent().unwrap()).unwrap();
        std::fs::write(
            &operator_path,
            format!(
                r#"
remotes:
  v2:
    name: v2
    url: https://user-level.example.com
    principal_id: {}
    signing_key: {}
    site_id: site:user
    vault_fingerprint: sha256:user
    ingest_ignore:
      patterns: []
  user-only:
    name: user-only
    url: https://user-only.example.com
    principal_id: {}
    signing_key: {}
    site_id: site:user-only
    vault_fingerprint: sha256:user-only
    ingest_ignore:
      patterns: []
"#,
                test_principal_id(5),
                test_signing_key(5),
                test_principal_id(6),
                test_signing_key(6),
            ),
        )
        .unwrap();

        let project_path = remotes_config_path(project_root.path());
        std::fs::create_dir_all(project_path.parent().unwrap()).unwrap();
        std::fs::write(
            &project_path,
            format!(
                r#"
remotes:
  v2:
    name: v2
    url: https://project-level.example.com
    principal_id: {}
    signing_key: {}
    site_id: site:project
    vault_fingerprint: sha256:project
    ingest_ignore:
      patterns: []
  project-only:
    name: project-only
    url: https://project-only.example.com
    principal_id: {}
    signing_key: {}
    site_id: site:project-only
    vault_fingerprint: sha256:project-only
    ingest_ignore:
      patterns: []
"#,
                test_principal_id(7),
                test_signing_key(7),
                test_principal_id(8),
                test_signing_key(8),
            ),
        )
        .unwrap();

        let merged = load_remotes_layered(app_root.path(), Some(project_root.path())).unwrap();
        assert_eq!(
            merged["v2"].url, "https://project-level.example.com",
            "project-level remotes must win on name collision"
        );
        assert!(merged.contains_key("user-only"));
        assert!(merged.contains_key("project-only"));
    }

    #[test]
    fn load_remotes_layered_survives_broken_operator_config() {
        let app_root = tempfile::tempdir().unwrap();
        let project_root = tempfile::tempdir().unwrap();

        let operator_path = remotes_config_path(app_root.path());
        std::fs::create_dir_all(operator_path.parent().unwrap()).unwrap();
        std::fs::write(
            &operator_path,
            r#"
remotes:
  broken:
    name: broken
    url: https://broken.example.com
    principal_id: fp:broken
    vault_fingerprint: sha256:broken
    ingest_ignore:
      patterns: []
"#,
        )
        .unwrap();

        let project_path = remotes_config_path(project_root.path());
        std::fs::create_dir_all(project_path.parent().unwrap()).unwrap();
        std::fs::write(
            &project_path,
            format!(
                r#"
remotes:
  v2:
    name: v2
    url: https://project-level.example.com
    principal_id: {}
    signing_key: {}
    site_id: site:project
    vault_fingerprint: sha256:project
    ingest_ignore:
      patterns: []
"#,
                test_principal_id(9),
                test_signing_key(9),
            ),
        )
        .unwrap();

        let merged = load_remotes_layered(app_root.path(), Some(project_root.path()))
            .expect("layered load must not fail when only user-level config has a malformed entry");
        assert!(
            merged.contains_key("v2"),
            "valid project-level remote must remain usable even when user-level entry is malformed: {merged:?}"
        );
        assert!(
            !merged.contains_key("broken"),
            "malformed user-level entry must be skipped"
        );
    }
}
