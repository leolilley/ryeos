//! Remote node configuration.
//!
//! A remote is a named connection target whose node key is pinned. Each pin
//! is one node-signed record, `<app_root>/.ai/node/remotes/<name>.yaml`
//! (schema `ryeos/remote-pin/v1`). The node key signs the record when the
//! local operator configures the remote; the loader verifies that signature
//! on every read, so a pinned key, URL or binding cannot be changed by
//! editing a file. Remote pins live only at the node level: a project never
//! contributes remotes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

pub use ryeos_core_tools::actions::remote_descriptor::{
    RemoteDescriptor, RemoteDescriptorError, RemoteDescriptorNode,
};
pub use ryeos_state::project_sync::ProjectSyncScope;

/// Schema of a node-signed remote pin record.
pub const REMOTE_PIN_SCHEMA: &str = "ryeos/remote-pin/v1";

/// Upper bound on one remote pin record.
const MAX_REMOTE_PIN_BYTES: u64 = 1024 * 1024;
/// Upper bound on the number of remote pin records.
const MAX_REMOTE_PINS: usize = 1024;

/// Validate a remote descriptor that has already been parsed and
/// signature-checked: its URL must be canonical and its pinned key must match
/// its fingerprint.
pub fn validate_descriptor(descriptor: &RemoteDescriptor) -> Result<()> {
    if descriptor.name.trim().is_empty() {
        anyhow::bail!("remote descriptor name must not be empty");
    }
    validate_url(&descriptor.url)?;
    ryeos_node_runtime::identity::validate_canonical_site_id(&descriptor.site_id)
        .context("remote descriptor site_id is not canonical")?;
    if descriptor.vault_fingerprint.trim().is_empty() {
        anyhow::bail!("remote descriptor vault_fingerprint must not be empty");
    }
    let key = decode_signing_key(&descriptor.node.public_key)
        .context("invalid remote descriptor node.public_key")?;
    let actual_fingerprint = lillux::crypto::fingerprint(&key);
    if descriptor.node.fingerprint != actual_fingerprint {
        anyhow::bail!(
            "remote descriptor fingerprint mismatch: node.fingerprint {}, public_key fingerprint {}",
            descriptor.node.fingerprint,
            actual_fingerprint,
        );
    }
    Ok(())
}

/// Read, verify and validate a signed remote descriptor file.
pub fn load_descriptor(path: &Path) -> Result<RemoteDescriptor> {
    let input = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read remote descriptor: {}", path.display()))?;
    let descriptor = RemoteDescriptor::parse_signed(&input)
        .map_err(anyhow::Error::new)
        .with_context(|| format!("invalid remote descriptor: {}", path.display()))?;
    validate_descriptor(&descriptor)
        .with_context(|| format!("invalid remote descriptor: {}", path.display()))?;
    Ok(descriptor)
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
/// `/public-key` endpoint and populates every field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteConfig {
    /// Human-readable name (also the record's file stem).
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
    /// Canonical local project path -> remote project binding. Operator-local.
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
        validate_remote_name(&self.name)?;
        validate_url(&self.url)?;
        self.pinned_signing_key()?;
        ryeos_node_runtime::identity::validate_canonical_site_id(&self.site_id)
            .with_context(|| format!("remote '{}' site_id is not canonical", self.name))?;
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

/// A remote name is also a file stem, so it is restricted to a safe alphabet.
pub fn validate_remote_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if !valid {
        anyhow::bail!(
            "remote name '{name}' must be 1-128 characters of letters, digits, '-', '_' or '.' and must not start with '.'"
        );
    }
    Ok(())
}

/// How the remote node key was pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinSource {
    Descriptor,
    ExpectedFingerprint,
    DescriptorAndExpectedFingerprint,
}

impl PinSource {
    pub fn as_str(self) -> &'static str {
        match self {
            PinSource::Descriptor => "descriptor",
            PinSource::ExpectedFingerprint => "expected_fingerprint",
            PinSource::DescriptorAndExpectedFingerprint => "descriptor_and_expected_fingerprint",
        }
    }
}

/// The signed on-disk record for one remote pin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemotePinRecord {
    schema: String,
    pin_source: PinSource,
    /// RFC 3339 time the pin was first made.
    pinned_at: String,
    /// Digest of the operator request that made the pin, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request: Option<String>,
    remote: RemoteConfig,
}

/// Why a remote pin or the remote layout was refused. Every variant has a
/// stable code and names the cure.
#[derive(Debug, thiserror::Error)]
pub enum RemotePinError {
    #[error(
        "remote_pin_unsigned: {path} carries no `# ryeos:signed:` line. Remote pins must be \
         signed by this node's key. Delete the file and re-pin the remote with \
         `ryeos remote configure <name> --url <url> --expect-fingerprint <fingerprint>`"
    )]
    Unsigned { path: PathBuf },

    #[error(
        "remote_pin_wrong_signer: {path} is signed by {signer}, not by this node's key {node}. \
         Only the node key may sign remote pins. Delete the file and re-pin the remote with \
         `ryeos remote configure <name> --url <url> --expect-fingerprint <fingerprint>`"
    )]
    WrongSigner {
        path: PathBuf,
        signer: String,
        node: String,
    },

    #[error(
        "remote_pin_tampered: {path} {reason}. The record was edited after signing or is \
         malformed. Delete the file and re-pin the remote with `ryeos remote configure <name> \
         --url <url> --expect-fingerprint <fingerprint>`"
    )]
    Tampered { path: PathBuf, reason: String },

    #[error(
        "remote_pin_invalid: {path} {reason}. Delete the file and re-pin the remote with \
         `ryeos remote configure <name> --url <url> --expect-fingerprint <fingerprint>`"
    )]
    Invalid { path: PathBuf, reason: String },

    #[error(
        "remote_config_file_unsupported: {path} is an unsigned remote config file this node no \
         longer reads. Remote pins are node-signed records under {records}. For each remote in \
         the file run `ryeos remote configure <name> --url <url> --expect-fingerprint \
         <fingerprint>`, then delete {path}"
    )]
    UnsupportedConfigFile { path: PathBuf, records: PathBuf },

    #[error("remote_pins_unreadable: {path} cannot be read securely: {reason}")]
    Unreadable { path: PathBuf, reason: String },
}

impl RemotePinError {
    /// Stable machine code.
    pub fn code(&self) -> &'static str {
        match self {
            RemotePinError::Unsigned { .. } => "remote_pin_unsigned",
            RemotePinError::WrongSigner { .. } => "remote_pin_wrong_signer",
            RemotePinError::Tampered { .. } => "remote_pin_tampered",
            RemotePinError::Invalid { .. } => "remote_pin_invalid",
            RemotePinError::UnsupportedConfigFile { .. } => "remote_config_file_unsupported",
            RemotePinError::Unreadable { .. } => "remote_pins_unreadable",
        }
    }
}

/// Turn a remote load failure into a structured handler refusal. A typed
/// remote pin refusal keeps its stable code and cure, as does an already
/// structured handler error; anything else goes through `fallback`.
pub fn typed_load_error_or(
    error: anyhow::Error,
    fallback: impl FnOnce(anyhow::Error) -> crate::handler_error::HandlerError,
) -> crate::handler_error::HandlerError {
    use crate::handler_error::HandlerError;
    if let Some(typed) = error.downcast_ref::<RemotePinError>() {
        return HandlerError::Structured {
            code: typed.code().to_string(),
            status: 409,
            body: serde_json::json!({ "code": typed.code(), "message": typed.to_string() }),
        };
    }
    match crate::handler_error::extract_handler_error(&error) {
        Some(typed @ HandlerError::Structured { .. }) => typed,
        _ => fallback(error),
    }
}

/// Where a loaded remote came from.
#[derive(Debug, Clone)]
pub struct LoadedRemote {
    pub config: RemoteConfig,
    pub config_path: PathBuf,
    pub pin_source: PinSource,
    pub pinned_at: String,
}

/// A remote pin that was present on disk but failed verification or
/// validation.
#[derive(Debug, Clone)]
pub struct InvalidRemote {
    pub name: String,
    pub config_path: PathBuf,
    /// Stable refusal code (`remote_pin_*`).
    pub code: String,
    pub error: String,
    pub url: Option<String>,
    pub repair_hint: String,
}

/// Remote load result with both usable configs and diagnostics.
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

/// Directory holding the node-signed remote pin records.
pub fn remote_pins_dir(app_root: &Path) -> PathBuf {
    ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf()).remote_pins_dir()
}

/// Path of one remote's pin record.
pub fn remote_pin_path(app_root: &Path, name: &str) -> PathBuf {
    remote_pins_dir(app_root).join(format!("{name}.yaml"))
}

/// The unsigned config file layout this node no longer reads. Its presence is
/// refused, because the entries it holds were never node-signed.
pub fn unsupported_config_file_path(app_root: &Path) -> PathBuf {
    app_root
        .join(ryeos_engine::AI_DIR)
        .join("config")
        .join("remotes")
        .join("remotes.yaml")
}

/// Refuse the unsupported config file when it exists.
pub fn refuse_unsupported_config_file(app_root: &Path) -> Result<(), RemotePinError> {
    let path = unsupported_config_file_path(app_root);
    match std::fs::symlink_metadata(&path) {
        Ok(_) => Err(RemotePinError::UnsupportedConfigFile {
            path,
            records: remote_pins_dir(app_root),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(RemotePinError::Unreadable {
            path,
            reason: error.to_string(),
        }),
    }
}

fn map_trust_error(error: ryeos_engine::trust::TrustError) -> RemotePinError {
    use ryeos_engine::trust::TrustError;
    match error {
        TrustError::RecordUnsigned { path } => RemotePinError::Unsigned { path },
        TrustError::RecordWrongSigner { path, signer, node } => {
            RemotePinError::WrongSigner { path, signer, node }
        }
        TrustError::RecordTampered { path, reason } => RemotePinError::Tampered { path, reason },
        TrustError::NodeIdentityUnavailable { path, reason } => {
            RemotePinError::Unreadable { path, reason }
        }
        other => RemotePinError::Invalid {
            path: remote_pins_dir(Path::new("")),
            reason: other.to_string(),
        },
    }
}

fn parse_remote_pin(
    path: &Path,
    bytes: &[u8],
    node_key: &lillux::crypto::VerifyingKey,
) -> Result<RemotePinRecord, RemotePinError> {
    let body = ryeos_engine::trust::verify_node_signed_text(path, bytes, node_key)
        .map_err(map_trust_error)?;
    let record: RemotePinRecord =
        serde_yaml::from_str(&body).map_err(|error| RemotePinError::Tampered {
            path: path.to_path_buf(),
            reason: format!("does not match the remote pin shape: {error}"),
        })?;
    if record.schema != REMOTE_PIN_SCHEMA {
        return Err(RemotePinError::Invalid {
            path: path.to_path_buf(),
            reason: format!(
                "declares schema `{}`, this node reads `{REMOTE_PIN_SCHEMA}`",
                record.schema
            ),
        });
    }
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    if stem != record.remote.name {
        return Err(RemotePinError::Invalid {
            path: path.to_path_buf(),
            reason: format!(
                "describes remote `{}` but is named `{stem}.yaml`",
                record.remote.name
            ),
        });
    }
    record
        .remote
        .validate()
        .map_err(|error| RemotePinError::Invalid {
            path: path.to_path_buf(),
            reason: format!("is not a valid remote: {error:#}"),
        })?;
    Ok(record)
}

/// Read every file under the pin directory. A missing directory is empty.
fn read_pin_files(dir: &Path) -> Result<Vec<(PathBuf, Vec<u8>)>, RemotePinError> {
    let unreadable = |reason: String| RemotePinError::Unreadable {
        path: dir.to_path_buf(),
        reason,
    };
    let mut files: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    lillux::visit_regular_files_no_follow_bounded(
        dir,
        lillux::DirectoryTraversalBudget::new(MAX_REMOTE_PINS + 1, 1),
        |_, is_directory| Ok(is_directory),
        |relative, file| {
            if files.len() >= MAX_REMOTE_PINS {
                anyhow::bail!("more than {MAX_REMOTE_PINS} remote pin records");
            }
            let bytes = lillux::read_open_regular_file_bounded(file, MAX_REMOTE_PIN_BYTES)?;
            files.push((dir.join(relative), bytes));
            Ok(())
        },
    )
    .map_err(|error| unreadable(format!("{error:#}")))?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

fn repair_hint(name: &str, url: Option<&str>) -> String {
    match url {
        Some(url) => format!(
            "run `ryeos remote configure {name} --url {url} --expect-fingerprint <fingerprint>` (or `--descriptor <file>`)"
        ),
        None => format!(
            "run `ryeos remote configure {name} --url <https-url> --expect-fingerprint <fingerprint>` (or `--descriptor <file>`)"
        ),
    }
}

/// Load every remote pin with diagnostics for the ones that fail
/// verification. Refuses outright when the unsupported config file exists.
///
/// A malformed or unsigned record is reported in `invalid` rather than
/// failing the whole load, so one stale record cannot block listing or using
/// the other remotes.
pub fn load_remotes_report(app_root: &Path) -> Result<RemotesLoadReport> {
    refuse_unsupported_config_file(app_root)?;
    let dir = remote_pins_dir(app_root);
    let mut report = RemotesLoadReport::default();
    let files = read_pin_files(&dir)?;
    if files.is_empty() {
        return Ok(report);
    }
    let node_key =
        ryeos_engine::trust::load_node_verifying_key(app_root).map_err(map_trust_error)?;
    for (path, bytes) in files {
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_string();
        let is_yaml = path.extension().and_then(|e| e.to_str()) == Some("yaml");
        let outcome = if is_yaml {
            parse_remote_pin(&path, &bytes, &node_key)
        } else {
            Err(RemotePinError::Invalid {
                path: path.clone(),
                reason: "is not a `<name>.yaml` remote pin record".to_string(),
            })
        };
        match outcome {
            Ok(record) => {
                report.remotes.insert(
                    record.remote.name.clone(),
                    LoadedRemote {
                        config: record.remote,
                        config_path: path,
                        pin_source: record.pin_source,
                        pinned_at: record.pinned_at,
                    },
                );
            }
            Err(error) => {
                tracing::warn!(
                    target: "ryeos_api::remote::config",
                    config_path = %path.display(),
                    remote = %name,
                    code = error.code(),
                    "skipping unusable remote pin record"
                );
                let diagnostic_url = serde_yaml::from_slice::<serde_yaml::Value>(&bytes)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("remote")?
                            .get("url")?
                            .as_str()
                            .map(ToOwned::to_owned)
                    })
                    .and_then(|raw| normalize_url(&raw).ok().filter(|n| *n == raw));
                report.invalid.push(InvalidRemote {
                    repair_hint: repair_hint(&name, diagnostic_url.as_deref()),
                    name,
                    config_path: path,
                    code: error.code().to_string(),
                    error: error.to_string(),
                    url: diagnostic_url,
                });
            }
        }
    }
    Ok(report)
}

/// Load the usable remote configs.
pub fn load_remotes(app_root: &Path) -> Result<HashMap<String, RemoteConfig>> {
    Ok(load_remotes_report(app_root)?
        .remotes
        .into_iter()
        .map(|(name, loaded)| (name, loaded.config))
        .collect())
}

/// Write one remote's node-signed pin record, replacing any previous record
/// for the name. The signature is made with the node key; callers must have
/// admitted the local operator before calling.
pub fn write_remote_pin(
    app_root: &Path,
    node_key: &lillux::crypto::SigningKey,
    config: &RemoteConfig,
    pin_source: PinSource,
    request_digest: Option<String>,
) -> Result<PathBuf> {
    config
        .validate()
        .with_context(|| format!("invalid remote '{}'", config.name))?;
    let dir = remote_pins_dir(app_root);
    let directory = lillux::PinnedDirectory::open_or_create(&dir)
        .with_context(|| format!("open remote pin directory {}", dir.display()))?;
    let _lock = directory.lock_exclusive()?;
    refuse_unsupported_config_file(app_root)?;
    let path = remote_pin_path(app_root, &config.name);
    let pinned_at = match read_pin_files(&dir)?
        .into_iter()
        .find(|(existing, _)| *existing == path)
    {
        Some((existing, bytes)) => {
            let verifying = node_key.verifying_key();
            parse_remote_pin(&existing, &bytes, &verifying)
                .map(|record| record.pinned_at)
                .unwrap_or_else(|_| lillux::time::iso8601_now())
        }
        None => lillux::time::iso8601_now(),
    };
    let record = RemotePinRecord {
        schema: REMOTE_PIN_SCHEMA.to_string(),
        pin_source,
        pinned_at,
        request: request_digest,
        remote: config.clone(),
    };
    let body = serde_yaml::to_string(&record).context("serialize remote pin record")?;
    let signed = lillux::signature::sign_content(&body, node_key, "#", None);
    lillux::atomic_write(&path, signed.as_bytes())
        .with_context(|| format!("write remote pin record {}", path.display()))?;
    Ok(path)
}

/// Replace the project bindings of an already pinned remote, keeping its
/// pin source and pin time, and re-sign the record.
pub fn rewrite_project_bindings(
    app_root: &Path,
    node_key: &lillux::crypto::SigningKey,
    name: &str,
    request_digest: Option<String>,
    mutate: impl FnOnce(&mut HashMap<String, RemoteProjectBinding>),
) -> Result<PathBuf> {
    let report = load_remotes_report(app_root)?;
    let loaded = get_loaded_remote(&report.remotes, name).map_err(|_| {
        anyhow::anyhow!(
            "remote '{name}' is not pinned on this node; run `ryeos remote configure {name} --url <url> --expect-fingerprint <fingerprint>` first"
        )
    })?;
    let mut config = loaded.config;
    mutate(&mut config.project_bindings);
    write_remote_pin(
        app_root,
        node_key,
        &config,
        loaded.pin_source,
        request_digest,
    )
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

/// Normalize one credential-free remote base URL. Non-loopback traffic must
/// use HTTPS; query, fragment, userinfo, and whitespace are never part of a
/// configured node authority.
pub fn normalize_url(url: &str) -> Result<String> {
    if url
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        anyhow::bail!("remote URL contains control or whitespace characters");
    }
    // Never copy rejected URL text into diagnostics: malformed userinfo can
    // fail parsing before the credential check below, and load diagnostics
    // are surfaced by both `remote list` and `remote doctor`.
    let mut parsed: url::Url = url.parse().context("invalid remote URL")?;

    if parsed.username() != "" || parsed.password().is_some() {
        anyhow::bail!("remote URL must not contain credentials");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        anyhow::bail!("remote base URL must not contain a query or fragment");
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("remote URL has no host"))?;
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
                "remote URL must use HTTPS except HTTP loopback (got scheme '{}')",
                other
            );
        }
    }
    let normalized_path = parsed.path().trim_end_matches('/').to_owned();
    parsed.set_path(&normalized_path);
    Ok(parsed.to_string().trim_end_matches('/').to_owned())
}

/// Require a stored or signed remote URL to already be in its one canonical
/// form. `remote configure` normalizes operator input before persistence;
/// descriptors and loaded config fail closed instead of silently moving their
/// signed/stored identity.
pub fn validate_url(url: &str) -> Result<()> {
    let normalized = normalize_url(url)?;
    if normalized != url {
        anyhow::bail!("remote URL is not canonical; reconfigure it using its exact base URL");
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
    fn normalize_rejects_secret_or_non_base_url_components() {
        assert!(normalize_url("https://user:secret@example.com").is_err());
        assert!(normalize_url("https://example.com?token=secret").is_err());
        assert!(normalize_url("https://example.com#secret").is_err());
        assert!(normalize_url("https://example.com/with space").is_err());
        assert_eq!(
            normalize_url("https://EXAMPLE.com/prefix/").unwrap(),
            "https://example.com/prefix"
        );
        assert!(validate_url("https://example.com/").is_err());
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

    fn node_fixture(app_root: &Path) -> lillux::crypto::SigningKey {
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(
            &ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf()).node_signing_key_path(),
        )
        .unwrap();
        identity
            .write_public_identity(&ryeos_engine::trust::node_public_identity_path(app_root))
            .unwrap();
        identity.signing_key().clone()
    }

    fn pinned_remote(name: &str, seed: u8) -> RemoteConfig {
        RemoteConfig {
            name: name.into(),
            url: "https://example.com".into(),
            principal_id: test_principal_id(seed),
            signing_key: test_signing_key(seed),
            site_id: "site:example".into(),
            vault_fingerprint: "sha256:def456".into(),
            ingest_ignore: ryeos_app::ignore::IgnoreConfig {
                patterns: vec![".git/".into(), "target/".into()],
            },
            project_bindings: HashMap::new(),
        }
    }

    fn signed_raw_record(key: &lillux::crypto::SigningKey, remote: RemoteConfig) -> String {
        let record = RemotePinRecord {
            schema: REMOTE_PIN_SCHEMA.to_string(),
            pin_source: PinSource::ExpectedFingerprint,
            pinned_at: "2026-01-01T00:00:00Z".into(),
            request: None,
            remote,
        };
        lillux::signature::sign_content(&serde_yaml::to_string(&record).unwrap(), key, "#", None)
    }

    #[test]
    fn remote_pin_record_signed_by_node_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let key = node_fixture(tmp.path());
        let path = write_remote_pin(
            tmp.path(),
            &key,
            &pinned_remote("default", 1),
            PinSource::Descriptor,
            Some("abc123".into()),
        )
        .unwrap();
        assert_eq!(path, remote_pin_path(tmp.path(), "default"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# ryeos:signed:"), "{text}");
        let report = load_remotes_report(tmp.path()).unwrap();
        assert!(report.invalid.is_empty(), "{:?}", report.invalid);
        let loaded = &report.remotes["default"];
        assert_eq!(loaded.config.url, "https://example.com");
        assert_eq!(loaded.config.site_id, "site:example");
        assert_eq!(loaded.config.ingest_ignore.patterns.len(), 2);
        assert_eq!(loaded.pin_source, PinSource::Descriptor);
        assert!(!loaded.pinned_at.is_empty());
        assert_eq!(load_remotes(tmp.path()).unwrap().len(), 1);
    }

    #[test]
    fn rewriting_a_pin_keeps_pin_time_and_source() {
        let tmp = tempfile::tempdir().unwrap();
        let key = node_fixture(tmp.path());
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        write_remote_pin(
            tmp.path(),
            &key,
            &pinned_remote("default", 1),
            PinSource::ExpectedFingerprint,
            None,
        )
        .unwrap();
        let before = load_remotes_report(tmp.path()).unwrap().remotes["default"]
            .pinned_at
            .clone();
        let canonical = canonical_local_project_path(&project).unwrap();
        let identity = local_project_identity(&canonical).unwrap().to_owned();
        rewrite_project_bindings(tmp.path(), &key, "default", None, |bindings| {
            bindings.insert(
                identity.clone(),
                RemoteProjectBinding {
                    remote_project_path: "/srv/project".into(),
                    sync_scope: ProjectSyncScope::default(),
                },
            );
        })
        .unwrap();
        let after = load_remotes_report(tmp.path()).unwrap();
        let loaded = &after.remotes["default"];
        assert_eq!(loaded.pinned_at, before);
        assert_eq!(loaded.pin_source, PinSource::ExpectedFingerprint);
        assert_eq!(
            loaded.config.project_bindings[&identity].remote_project_path,
            "/srv/project"
        );
    }

    #[test]
    fn unsigned_remote_pin_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let key = node_fixture(tmp.path());
        let path = write_remote_pin(
            tmp.path(),
            &key,
            &pinned_remote("default", 1),
            PinSource::Descriptor,
            None,
        )
        .unwrap();
        let signed = std::fs::read_to_string(&path).unwrap();
        let unsigned: String = signed
            .lines()
            .filter(|line| !line.contains("ryeos:signed:"))
            .map(|line| format!("{line}\n"))
            .collect();
        std::fs::write(&path, unsigned).unwrap();
        let report = load_remotes_report(tmp.path()).unwrap();
        assert!(report.remotes.is_empty());
        assert_eq!(report.invalid.len(), 1);
        assert_eq!(report.invalid[0].code, "remote_pin_unsigned");
        assert!(
            report.invalid[0]
                .repair_hint
                .contains("ryeos remote configure default")
        );
    }

    #[test]
    fn edited_signing_key_is_detected() {
        let tmp = tempfile::tempdir().unwrap();
        let key = node_fixture(tmp.path());
        let path = write_remote_pin(
            tmp.path(),
            &key,
            &pinned_remote("default", 1),
            PinSource::Descriptor,
            None,
        )
        .unwrap();
        let signed = std::fs::read_to_string(&path).unwrap();
        let edited = signed.replace(&test_signing_key(1), &test_signing_key(2));
        assert_ne!(edited, signed);
        std::fs::write(&path, edited).unwrap();
        let report = load_remotes_report(tmp.path()).unwrap();
        assert!(report.remotes.is_empty());
        assert_eq!(report.invalid[0].code, "remote_pin_tampered");
    }

    #[test]
    fn pin_signed_by_another_key_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        node_fixture(tmp.path());
        let other = lillux::crypto::SigningKey::from_bytes(&[9; 32]);
        let dir = remote_pins_dir(tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("default.yaml"),
            signed_raw_record(&other, pinned_remote("default", 1)),
        )
        .unwrap();
        let report = load_remotes_report(tmp.path()).unwrap();
        assert!(report.remotes.is_empty());
        assert_eq!(report.invalid[0].code, "remote_pin_wrong_signer");
    }

    #[test]
    fn pin_named_for_another_remote_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let key = node_fixture(tmp.path());
        let dir = remote_pins_dir(tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("alias.yaml"),
            signed_raw_record(&key, pinned_remote("default", 1)),
        )
        .unwrap();
        let report = load_remotes_report(tmp.path()).unwrap();
        assert!(report.remotes.is_empty());
        assert_eq!(report.invalid[0].code, "remote_pin_invalid");
    }

    #[test]
    fn unsupported_config_file_is_refused_with_cure() {
        let tmp = tempfile::tempdir().unwrap();
        let key = node_fixture(tmp.path());
        let stale = unsupported_config_file_path(tmp.path());
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, "remotes: {}\n").unwrap();
        for error in [
            load_remotes_report(tmp.path()).unwrap_err(),
            write_remote_pin(
                tmp.path(),
                &key,
                &pinned_remote("default", 1),
                PinSource::Descriptor,
                None,
            )
            .unwrap_err(),
        ] {
            let typed = error
                .downcast_ref::<RemotePinError>()
                .expect("typed remote pin error");
            assert_eq!(typed.code(), "remote_config_file_unsupported");
            let message = typed.to_string();
            assert!(message.contains("ryeos remote configure"), "{message}");
            assert!(message.contains("then delete"), "{message}");
        }
    }

    #[test]
    fn remote_names_are_safe_file_stems() {
        for bad in ["", ".hidden", "a/b", "a b", "../x"] {
            assert!(validate_remote_name(bad).is_err(), "{bad:?}");
        }
        for good in ["default", "gpu-1", "prod_eu.1"] {
            assert!(validate_remote_name(good).is_ok(), "{good:?}");
        }
    }

    fn assert_credential_url_not_retained(url: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let key = node_fixture(tmp.path());
        let mut remote = pinned_remote("unsafe", 11);
        remote.url = url.to_string();
        let dir = remote_pins_dir(tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("unsafe.yaml"), signed_raw_record(&key, remote)).unwrap();
        let report = load_remotes_report(tmp.path()).unwrap();
        assert!(report.remotes.is_empty());
        assert_eq!(report.invalid.len(), 1);
        assert!(report.invalid[0].url.is_none());
        assert!(!report.invalid[0].repair_hint.contains("do-not-retain"));
        assert!(!report.invalid[0].error.contains("do-not-retain"));
    }

    #[test]
    fn load_rejects_credential_url_without_retaining_it_in_diagnostics() {
        assert_credential_url_not_retained("https://user:do-not-retain@example.com");
    }

    #[test]
    fn load_rejects_malformed_credential_url_without_retaining_it_in_diagnostics() {
        assert_credential_url_not_retained("https://user:do-not-retain@[");
    }

    #[test]
    fn descriptor_requires_canonical_bound_fields() {
        let key = lillux::crypto::SigningKey::from_bytes(&[7; 32]);
        let mut descriptor = RemoteDescriptor {
            version: 2,
            name: "hosted-prod".into(),
            url: "https://node.example.com".into(),
            site_id: format!("site:{}", test_fingerprint(7)),
            vault_fingerprint: "sha256:abc".into(),
            node: RemoteDescriptorNode {
                public_key: test_signing_key(7),
                fingerprint: test_fingerprint(7),
            },
            capabilities: vec![],
            admission: None,
            provider: None,
        };
        let signed = descriptor.render_signed(&key).unwrap();
        let parsed = RemoteDescriptor::parse_signed(&signed).unwrap();
        validate_descriptor(&parsed).unwrap();
        descriptor.node.fingerprint = test_fingerprint(8);
        let mismatch = descriptor.render_signed(&key).unwrap();
        assert!(RemoteDescriptor::parse_signed(&mismatch).is_err());
    }
}
