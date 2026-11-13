//! Remote node configuration.
//!
//! Remotes are named connection targets stored in YAML. The config is
//! loaded from `<app_root>/.ai/config/remotes/remotes.yaml`.
//!
//! A remote entry pins the remote node's identity (`signing_key`,
//! `principal_id`, `vault_fingerprint`). Only the node's own remotes file may
//! hold that trust material. A project's `.ai/config/remotes/remotes.yaml`
//! that names any of it is refused with [`PROJECT_REMOTE_TRUST_MATERIAL`];
//! the cure is `ryeos remote configure` on the node. Because every usable
//! remote needs a pinned key, a project layer can no longer define or
//! override a remote; its other entries are reported as invalid.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine as _;
use serde::de::Error as _;
use serde::{Deserialize, Serialize};

use crate::handler_error::HandlerError;

pub use ryeos_state::project_sync::ProjectSyncScope;

/// Stable refusal code: a project's remotes file names remote trust
/// material, which only the node's own remotes config may hold.
pub const PROJECT_REMOTE_TRUST_MATERIAL: &str = "project_remote_trust_material";

/// Remote entry fields that pin a remote node's identity. A project layer
/// may neither supply nor override any of them.
pub const REMOTE_TRUST_FIELDS: &[&str] = &[
    "signing_key",
    "principal_id",
    "fingerprint",
    "vault_fingerprint",
    "descriptor",
];

/// Stable refusal code: a descriptor lacks a field `remote pin` needs to
/// write a complete remote entry without contacting the remote.
pub const REMOTE_DESCRIPTOR_INCOMPLETE: &str = "remote_descriptor_incomplete";
/// Stable refusal code: the remote was pinned from a descriptor that carried
/// no ingest rules, so nothing may be pushed to it.
pub const REMOTE_PINNED_WITHOUT_INGEST_RULES: &str = "remote_pinned_without_ingest_rules";

/// Provider/import descriptor for a remote RyeOS node.
///
/// A descriptor is a trust pin and discovery convenience, not a credential.
/// Importing one records the expected node identity for `remote configure`,
/// which still verifies the live node before writing local remote config.
/// `remote pin` writes the entry from the descriptor alone, so it also needs
/// the node's site and vault fingerprint; the ingest rules are optional and
/// their absence means nothing is pushed to the pinned remote.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RemoteDescriptor {
    pub version: u32,
    pub name: Option<String>,
    pub url: String,
    pub node: RemoteDescriptorNode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingest_ignore: Option<ryeos_app::ignore::IgnoreConfig>,
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
    /// The node's site, `site:<fingerprint>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_id: Option<String>,
    /// The fingerprint of the node's vault public key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_fingerprint: Option<String>,
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
        if let Some(name) = &self.name
            && name.trim().is_empty()
        {
            anyhow::bail!("remote descriptor name must not be empty when present");
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
        if let Some(site_id) = &self.node.site_id
            && *site_id != format!("site:{actual_fingerprint}")
        {
            anyhow::bail!(
                "remote descriptor node.site_id {site_id} is not the site of node {actual_fingerprint}"
            );
        }
        Ok(())
    }

    /// The complete remote entry this descriptor pins under `name`, with no
    /// contact with the remote. A descriptor without the node's site or vault
    /// fingerprint is refused with [`REMOTE_DESCRIPTOR_INCOMPLETE`].
    pub fn pinned_remote_config(&self, name: &str) -> Result<RemoteConfig, HandlerError> {
        self.validate()
            .map_err(|error| HandlerError::BadRequest(format!("{error:#}")))?;
        let missing: Vec<&str> = [
            ("node.site_id", self.node.site_id.is_none()),
            (
                "node.vault_fingerprint",
                self.node.vault_fingerprint.is_none(),
            ),
        ]
        .into_iter()
        .filter_map(|(field, absent)| absent.then_some(field))
        .collect();
        if !missing.is_empty() {
            return Err(HandlerError::Structured {
                code: REMOTE_DESCRIPTOR_INCOMPLETE.to_owned(),
                status: 400,
                body: serde_json::json!({
                    "code": REMOTE_DESCRIPTOR_INCOMPLETE,
                    "message": format!(
                        "the descriptor for '{name}' lacks {}, which pinning without contacting the remote needs; export it again with `ryeos remote-descriptor` from a current node",
                        missing.join(" and ")
                    ),
                    "remote": name,
                    "missing": missing,
                }),
            });
        }
        let config = RemoteConfig {
            name: name.to_owned(),
            url: normalize_url(&self.url)
                .map_err(|error| HandlerError::BadRequest(format!("{error:#}")))?,
            principal_id: format!("fp:{}", self.node.fingerprint),
            signing_key: self.node.public_key.clone(),
            site_id: self.node.site_id.clone().unwrap_or_default(),
            vault_fingerprint: self.node.vault_fingerprint.clone().unwrap_or_default(),
            ingest_ignore: self.ingest_ignore.clone(),
            project_bindings: HashMap::new(),
        };
        config
            .validate()
            .map_err(|error| HandlerError::BadRequest(format!("{error:#}")))?;
        Ok(config)
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
    /// `remote configure`. Push needs it to use the correct ignore rules;
    /// re-run `remote configure` if stale. A remote pinned from a
    /// descriptor that carried none has none, and nothing is pushed to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingest_ignore: Option<ryeos_app::ignore::IgnoreConfig>,
    /// Canonical local project path -> remote project binding.
    ///
    /// These bindings are operator-local and are written only to the
    /// node's own remotes file, like every other remote field.
    #[serde(default)]
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub project_bindings: HashMap<String, RemoteProjectBinding>,
}

impl RemoteConfig {
    /// The remote's ingest rules, or the typed refusal
    /// [`REMOTE_PINNED_WITHOUT_INGEST_RULES`] for a remote pinned without
    /// them.
    pub fn ingest_rules(&self) -> Result<&ryeos_app::ignore::IgnoreConfig, HandlerError> {
        self.ingest_ignore
            .as_ref()
            .ok_or_else(|| HandlerError::Structured {
                code: REMOTE_PINNED_WITHOUT_INGEST_RULES.to_owned(),
                status: 409,
                body: serde_json::json!({
                    "code": REMOTE_PINNED_WITHOUT_INGEST_RULES,
                    "message": format!(
                        "remote '{}' was pinned from a descriptor without ingest rules, so this node pushes nothing to it; run `ryeos remote configure {}` once the remote is reachable, or pin a descriptor that carries them",
                        self.name, self.name
                    ),
                    "remote": self.name,
                }),
            })
    }

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
/// stale entries must not block listing or using other valid remotes.
pub fn load_remotes(app_root: &Path) -> Result<HashMap<String, RemoteConfig>> {
    let path = remotes_config_path(app_root);
    load_remotes_at(&path)
}

/// Load a remotes file from an explicit absolute path. Same per-entry
/// resilience as [`load_remotes`].
pub fn load_remotes_at(path: &Path) -> Result<HashMap<String, RemoteConfig>> {
    Ok(load_remotes_at_report(path)?
        .remotes
        .into_iter()
        .map(|(name, loaded)| (name, loaded.config))
        .collect())
}

/// Load the node's own remotes file at `path`.
fn load_remotes_at_report(path: &Path) -> Result<RemotesLoadReport> {
    if !path.exists() {
        return Ok(RemotesLoadReport::default());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read remotes config: {}", path.display()))?;
    parse_remotes_report(&content, path)
}

/// Upper bound on a project-layer remotes file read through a retained
/// project directory.
const MAX_PROJECT_REMOTES_CONFIG_BYTES: u64 = 1024 * 1024;

/// Read the project-layer remotes file beneath the exact open `project`
/// directory. Every component is traversed from the held descriptor without
/// following symlinks; the project's pathname is never reopened.
fn load_project_remotes_report_under_authority(
    project: &lillux::PinnedDirectory,
) -> Result<RemotesLoadReport> {
    let root = project.path().to_path_buf();
    let path = remotes_config_path(&root);
    let mut relative = PathBuf::from(ryeos_engine::AI_DIR);
    for seg in REMOTES_CONFIG_SUBPATH {
        relative.push(seg);
    }
    let Some(file) = project
        .open_pinned_regular_descendant(&relative, false)
        .with_context(|| format!("failed to open remotes config: {}", path.display()))?
    else {
        return Ok(RemotesLoadReport::default());
    };
    let bytes = file
        .read_bounded(MAX_PROJECT_REMOTES_CONFIG_BYTES)
        .with_context(|| format!("failed to read remotes config: {}", path.display()))?;
    let content = String::from_utf8(bytes)
        .with_context(|| format!("remotes config is not UTF-8: {}", path.display()))?;
    parse_project_remotes_report(&content, &path, root)
}

/// Read a project-layer remotes file.
///
/// A project may not supply or override remote trust material: any entry
/// naming a field in [`REMOTE_TRUST_FIELDS`] refuses the whole load with
/// [`PROJECT_REMOTE_TRUST_MATERIAL`]. Every other entry cannot form a usable
/// remote (a remote needs a pinned key) and is reported as invalid, so the
/// returned report never carries remotes.
fn parse_project_remotes_report(
    content: &str,
    path: &Path,
    root: PathBuf,
) -> Result<RemotesLoadReport> {
    let file: RemotesFile = serde_yaml::from_str(content)
        .with_context(|| format!("invalid remotes config: {}", path.display()))?;
    let mut names: Vec<&String> = file.remotes.keys().collect();
    names.sort();
    let mut offenders: Vec<(String, Vec<&'static str>)> = Vec::new();
    for name in &names {
        let raw = &file.remotes[*name];
        let fields: Vec<&'static str> = REMOTE_TRUST_FIELDS
            .iter()
            .copied()
            .filter(|field| raw.get(*field).is_some())
            .collect();
        if !fields.is_empty() {
            offenders.push(((*name).clone(), fields));
        }
    }
    if !offenders.is_empty() {
        return Err(project_remote_trust_refusal(path, &offenders));
    }
    let scope = RemoteConfigScope::Project { root };
    let mut report = RemotesLoadReport::default();
    for name in names {
        report.invalid.push(InvalidRemote {
            name: name.clone(),
            scope: scope.clone(),
            config_path: path.to_path_buf(),
            error: "a project remotes file cannot define a remote: a remote needs a pinned node key, and only the node's own remotes config may hold one".to_string(),
            url: None,
            repair_hint: format!(
                "remove '{}' from {} and configure it on the node with `ryeos remote configure {} --url <https-url> --expect-fingerprint <fingerprint>`",
                name,
                path.display(),
                name,
            ),
        });
    }
    Ok(report)
}

/// The typed refusal for a project remotes file that names trust material.
fn project_remote_trust_refusal(
    path: &Path,
    offenders: &[(String, Vec<&'static str>)],
) -> anyhow::Error {
    let names: Vec<&str> = offenders.iter().map(|(name, _)| name.as_str()).collect();
    let remotes: Vec<serde_json::Value> = offenders
        .iter()
        .map(|(name, fields)| serde_json::json!({ "name": name, "fields": fields }))
        .collect();
    let cures: Vec<String> = offenders
        .iter()
        .map(|(name, _)| {
            format!(
                "ryeos remote configure {name} --url <https-url> --expect-fingerprint <fingerprint>"
            )
        })
        .collect();
    let message = format!(
        "{PROJECT_REMOTE_TRUST_MATERIAL}: project remotes file {} names remote trust material for [{}]; a project may not supply or override a remote's signing_key, principal_id, fingerprint, vault_fingerprint or descriptor. Remove those entries from the project file and configure the remote on the node with `ryeos remote configure <name> --url <https-url> --expect-fingerprint <fingerprint>`",
        path.display(),
        names.join(", "),
    );
    anyhow::Error::new(HandlerError::Structured {
        code: PROJECT_REMOTE_TRUST_MATERIAL.to_string(),
        status: 409,
        body: serde_json::json!({
            "code": PROJECT_REMOTE_TRUST_MATERIAL,
            "message": message,
            "config_path": path.display().to_string(),
            "remotes": remotes,
            "cures": cures,
        }),
    })
    .context(message)
}

/// Keep a typed refusal from a remotes load (such as
/// [`PROJECT_REMOTE_TRUST_MATERIAL`]); map any other failure with `fallback`.
pub fn typed_load_error_or(
    error: anyhow::Error,
    fallback: impl FnOnce(anyhow::Error) -> HandlerError,
) -> HandlerError {
    match crate::handler_error::extract_handler_error(&error) {
        Some(typed @ HandlerError::Structured { .. }) => typed,
        _ => fallback(error),
    }
}

fn parse_remotes_report(content: &str, path: &Path) -> Result<RemotesLoadReport> {
    let mut report = RemotesLoadReport::default();
    let file: RemotesFile = serde_yaml::from_str(content)
        .with_context(|| format!("invalid remotes config: {}", path.display()))?;
    for (name, raw) in file.remotes {
        let raw_url = raw
            .get("url")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned);
        let diagnostic_url = raw_url
            .as_deref()
            .and_then(|url| normalize_url(url).ok())
            .filter(|normalized| raw_url.as_deref() == Some(normalized.as_str()));
        match serde_yaml::from_value::<RemoteConfig>(raw).and_then(|cfg| {
            if cfg.name != name {
                return Err(serde_yaml::Error::custom(format!(
                    "remote map key '{}' does not match remote.name '{}'",
                    name, cfg.name,
                )));
            }
            cfg.validate()
                .map_err(|e| serde_yaml::Error::custom(format!("{e:#}")))?;
            Ok(cfg)
        }) {
            Ok(cfg) => {
                report.remotes.insert(
                    name,
                    LoadedRemote {
                        config: cfg,
                        scope: RemoteConfigScope::Operator,
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
                let repair_hint = match diagnostic_url.as_deref() {
                    Some(url) => format!(
                        "run `ryeos remote configure {} --url {} --expect-fingerprint <fingerprint>` (or `--descriptor <file>`)",
                        name, url
                    ),
                    None => format!(
                        "run `ryeos remote configure {} --url <https-url> --expect-fingerprint <fingerprint>` (or `--descriptor <file>`)",
                        name
                    ),
                };
                report.invalid.push(InvalidRemote {
                    name,
                    scope: RemoteConfigScope::Operator,
                    config_path: path.to_path_buf(),
                    error: e.to_string(),
                    url: diagnostic_url,
                    repair_hint,
                });
            }
        }
    }
    Ok(report)
}

/// Load the node's remotes, checking an optional project layer.
///
/// Loads the operator-level remotes file from `app_root`. If `project_path`
/// is provided and `<project>/.ai/config/remotes/remotes.yaml` exists, it is
/// checked: an entry naming remote trust material refuses the load with
/// [`PROJECT_REMOTE_TRUST_MATERIAL`], and any other entry is a diagnostic.
/// A project layer never supplies or overrides a remote. Individually
/// malformed node entries do not prevent the others from being used.
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
    let mut report = load_remotes_at_report(&remotes_config_path(app_root))?;
    if let Some(project) = project_path {
        let canonical_project = canonical_local_project_path(project)?;
        let project_path = remotes_config_path(&canonical_project);
        if !is_node_root(app_root, &canonical_project) && project_path.exists() {
            let content = std::fs::read_to_string(&project_path).with_context(|| {
                format!("failed to read remotes config: {}", project_path.display())
            })?;
            let project_report =
                parse_project_remotes_report(&content, &project_path, canonical_project)?;
            layer_project_remotes(&mut report, project_report);
        }
    }
    Ok(report)
}

/// Load operator remotes plus the project layer of a retained project
/// directory, retaining invalid entries as diagnostics.
///
/// The project layer is read only through `project`'s open descriptor, so a
/// directory substituted at the project's pathname after `project` was opened
/// cannot redirect the read. The project scope records `project.path()`, the
/// identity the directory was bound to, for display only.
pub fn load_remotes_layered_report_under_project_authority(
    app_root: &Path,
    project: Option<&lillux::PinnedDirectory>,
) -> Result<RemotesLoadReport> {
    let mut report = load_remotes_at_report(&remotes_config_path(app_root))?;
    if let Some(project) = project
        && !is_node_root(app_root, project.path())
    {
        let project_report = load_project_remotes_report_under_authority(project)?;
        layer_project_remotes(&mut report, project_report);
    }
    Ok(report)
}

/// Whether `project` is the node's own root, whose remotes file is the
/// node's and was already loaded as the operator layer.
fn is_node_root(app_root: &Path, project: &Path) -> bool {
    match app_root.canonicalize() {
        Ok(app_root) => app_root == project,
        Err(_) => app_root == project,
    }
}

/// [`load_remotes_layered_report_under_project_authority`] reduced to the
/// usable remote configs.
pub fn load_remotes_layered_under_project_authority(
    app_root: &Path,
    project: Option<&lillux::PinnedDirectory>,
) -> Result<HashMap<String, RemoteConfig>> {
    Ok(
        load_remotes_layered_report_under_project_authority(app_root, project)?
            .remotes
            .into_iter()
            .map(|(name, loaded)| (name, loaded.config))
            .collect(),
    )
}

/// Add a project layer's diagnostics to the node's report. A project layer
/// never supplies or overrides a remote (see [`parse_project_remotes_report`]),
/// so only its invalid entries carry over.
fn layer_project_remotes(report: &mut RemotesLoadReport, project_report: RemotesLoadReport) {
    debug_assert!(project_report.remotes.is_empty());
    report.invalid.extend(project_report.invalid);
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

/// Pin a remote from its descriptor in the node's own remotes file, without
/// contacting it: the descriptor is the trust anchor. An existing entry of
/// the same name is replaced; its project bindings are kept. This is the one
/// core behind `ryeos remote pin` and the offline operation
/// `node.remote.pin`.
pub fn pin_remote(
    app_root: &Path,
    name: &str,
    descriptor: &RemoteDescriptor,
) -> Result<serde_json::Value, HandlerError> {
    let mut config = descriptor.pinned_remote_config(name)?;
    let internal = |error: anyhow::Error| HandlerError::Internal(format!("{error:#}"));
    let mut remotes = load_remotes(app_root).map_err(internal)?;
    if let Some(existing) = remotes.get(name) {
        config.project_bindings = existing.project_bindings.clone();
    }
    remotes.insert(name.to_owned(), config.clone());
    save_remotes(app_root, &remotes).map_err(internal)?;
    Ok(serde_json::json!({
        "pinned": name,
        "config_path": remotes_config_path(app_root),
        "url": config.url,
        "principal_id": config.principal_id,
        "site_id": config.site_id,
        "vault_fingerprint": config.vault_fingerprint,
        "ingest_rules": config.ingest_ignore.is_some(),
    }))
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

    fn pin_descriptor(seed: u8, site: bool, vault: bool, ingest: bool) -> RemoteDescriptor {
        let key = test_signing_key(seed);
        let fingerprint = lillux::crypto::fingerprint(&decode_signing_key(&key).unwrap());
        RemoteDescriptor {
            version: 1,
            name: Some("origin".into()),
            url: "https://origin.example.com".into(),
            node: RemoteDescriptorNode {
                public_key: key,
                site_id: site.then(|| format!("site:{fingerprint}")),
                vault_fingerprint: vault.then(|| "sha256:origin-vault".to_owned()),
                fingerprint,
            },
            ingest_ignore: ingest.then(|| ryeos_app::ignore::IgnoreConfig {
                patterns: vec!["target/".into()],
            }),
            capabilities: None,
            admission: None,
            provider: None,
        }
    }

    fn structured_code(error: HandlerError) -> String {
        match error {
            HandlerError::Structured { code, .. } => code,
            other => panic!("expected a typed refusal, got {other:?}"),
        }
    }

    #[test]
    fn remote_pin_operation_writes_complete_entry_without_dial() {
        let tmpdir = tempfile::tempdir().unwrap();
        let descriptor = pin_descriptor(9, true, true, true);
        let report = pin_remote(tmpdir.path(), "origin", &descriptor).unwrap();
        assert_eq!(report["ingest_rules"], true);
        let loaded = load_remotes(tmpdir.path()).unwrap();
        let pinned = &loaded["origin"];
        assert_eq!(pinned.signing_key, descriptor.node.public_key);
        assert_eq!(
            pinned.principal_id,
            format!("fp:{}", descriptor.node.fingerprint)
        );
        assert_eq!(pinned.site_id, descriptor.node.site_id.clone().unwrap());
        assert_eq!(pinned.vault_fingerprint, "sha256:origin-vault");
        assert_eq!(pinned.ingest_rules().unwrap().patterns, ["target/"]);
        // Pinning again converges on the same entry.
        pin_remote(tmpdir.path(), "origin", &descriptor).unwrap();
        assert_eq!(load_remotes(tmpdir.path()).unwrap().len(), 1);
    }

    #[test]
    fn remote_pin_refuses_descriptor_missing_site() {
        let tmpdir = tempfile::tempdir().unwrap();
        for descriptor in [
            pin_descriptor(9, false, true, true),
            pin_descriptor(9, true, false, true),
        ] {
            let refusal = pin_remote(tmpdir.path(), "origin", &descriptor).unwrap_err();
            assert_eq!(structured_code(refusal), REMOTE_DESCRIPTOR_INCOMPLETE);
        }
        assert!(load_remotes(tmpdir.path()).unwrap().is_empty());
        // A site that is not the node's own is refused outright.
        let mut foreign = pin_descriptor(9, true, true, true);
        foreign.node.site_id = Some("site:somewhere-else".into());
        assert!(pin_remote(tmpdir.path(), "origin", &foreign).is_err());
    }

    #[test]
    fn push_to_pinned_remote_without_ingest_rules_refused() {
        let tmpdir = tempfile::tempdir().unwrap();
        let report = pin_remote(
            tmpdir.path(),
            "origin",
            &pin_descriptor(9, true, true, false),
        )
        .unwrap();
        assert_eq!(report["ingest_rules"], false);
        let loaded = load_remotes(tmpdir.path()).unwrap();
        let refusal = loaded["origin"].ingest_rules().unwrap_err();
        assert_eq!(structured_code(refusal), REMOTE_PINNED_WITHOUT_INGEST_RULES);
    }
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

    #[test]
    fn load_rejects_credential_url_without_retaining_it_in_diagnostics() {
        let tmpdir = tempfile::tempdir().unwrap();
        let path = remotes_config_path(tmpdir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                r#"
remotes:
  unsafe:
    name: unsafe
    url: https://user:do-not-retain@example.com
    principal_id: {}
    signing_key: {}
    site_id: site:unsafe
    vault_fingerprint: sha256:unsafe
    ingest_ignore:
      patterns: []
"#,
                test_principal_id(11),
                test_signing_key(11),
            ),
        )
        .unwrap();

        let report = load_remotes_at_report(&path).unwrap();
        assert!(report.remotes.is_empty());
        assert_eq!(report.invalid.len(), 1);
        assert!(report.invalid[0].url.is_none());
        assert!(!report.invalid[0].repair_hint.contains("do-not-retain"));
        assert!(!report.invalid[0].error.contains("do-not-retain"));
    }

    #[test]
    fn load_rejects_malformed_credential_url_without_retaining_it_in_diagnostics() {
        let tmpdir = tempfile::tempdir().unwrap();
        let path = remotes_config_path(tmpdir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                r#"
remotes:
  unsafe:
    name: unsafe
    url: 'https://user:do-not-retain@['
    principal_id: {}
    signing_key: {}
    site_id: site:unsafe
    vault_fingerprint: sha256:unsafe
    ingest_ignore:
      patterns: []
"#,
                test_principal_id(12),
                test_signing_key(12),
            ),
        )
        .unwrap();

        let report = load_remotes_at_report(&path).unwrap();
        assert!(report.remotes.is_empty());
        assert_eq!(report.invalid.len(), 1);
        assert!(report.invalid[0].url.is_none());
        assert!(!report.invalid[0].repair_hint.contains("do-not-retain"));
        assert!(!report.invalid[0].error.contains("do-not-retain"));
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
                ingest_ignore: Some(ryeos_app::ignore::IgnoreConfig {
                    patterns: vec![".git/".into(), "target/".into()],
                }),
                project_bindings: HashMap::new(),
            },
        );
        save_remotes(tmpdir.path(), &remotes).unwrap();
        let loaded = load_remotes(tmpdir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["default"].url, "https://example.com");
        assert_eq!(loaded["default"].site_id, "site:example");
        assert_eq!(loaded["default"].vault_fingerprint, "sha256:def456");
        assert_eq!(
            loaded["default"]
                .ingest_ignore
                .as_ref()
                .unwrap()
                .patterns
                .len(),
            2
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
            "example-remote".into(),
            RemoteConfig {
                name: "example-remote".into(),
                url: "https://example.com".into(),
                principal_id: test_principal_id(2),
                signing_key: test_signing_key(2),
                site_id: "site:example".into(),
                vault_fingerprint: "sha256:def456".into(),
                ingest_ignore: Some(ryeos_app::ignore::IgnoreConfig {
                    patterns: vec![".git/".into()],
                }),
                project_bindings: bindings,
            },
        );

        save_remotes(tmpdir.path(), &remotes).unwrap();
        let loaded = load_remotes(tmpdir.path()).unwrap();
        let resolved = resolve_project_binding(&loaded["example-remote"], &local).unwrap();
        assert_eq!(resolved.remote_project_path, "/data/projects/example");
        assert_eq!(resolved.sync_scope, ProjectSyncScope::AiOnly);
    }

    fn write_project_remotes(project_root: &Path, content: &str) -> PathBuf {
        let path = remotes_config_path(project_root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    fn node_remote(name: &str, seed: u8) -> RemoteConfig {
        RemoteConfig {
            name: name.into(),
            url: format!("https://{name}.node.example.com"),
            principal_id: test_principal_id(seed),
            signing_key: test_signing_key(seed),
            site_id: format!("site:{name}"),
            vault_fingerprint: format!("sha256:{name}"),
            ingest_ignore: Some(ryeos_app::ignore::IgnoreConfig { patterns: vec![] }),
            project_bindings: HashMap::new(),
        }
    }

    fn structured_refusal(err: &anyhow::Error) -> (String, u16, serde_json::Value) {
        match crate::handler_error::extract_handler_error(err) {
            Some(HandlerError::Structured { code, status, body }) => (code, status, body),
            other => panic!("expected a structured refusal, got {other:?} from {err:#}"),
        }
    }

    #[test]
    fn project_remote_cannot_override_node_signing_key() {
        let app_root = tempfile::tempdir().unwrap();
        let project_root = tempfile::tempdir().unwrap();
        let project_root_path = project_root.path().canonicalize().unwrap();
        let mut node = HashMap::new();
        node.insert("example".to_string(), node_remote("example", 3));
        save_remotes(app_root.path(), &node).unwrap();

        let project_path = write_project_remotes(
            &project_root_path,
            &format!(
                r#"
category: remotes
remotes:
  example:
    name: example
    url: https://attacker.example.com
    principal_id: {}
    signing_key: {}
    site_id: site:example
    vault_fingerprint: sha256:attacker
    ingest_ignore:
      patterns: []
"#,
                test_principal_id(4),
                test_signing_key(4),
            ),
        );

        let err = load_remotes_layered_report(app_root.path(), Some(&project_root_path))
            .expect_err("a project may not override a node remote's pinned key");
        let (code, status, body) = structured_refusal(&err);
        assert_eq!(code, PROJECT_REMOTE_TRUST_MATERIAL);
        assert_eq!(status, 409);
        assert_eq!(body["code"], PROJECT_REMOTE_TRUST_MATERIAL);
        assert_eq!(body["config_path"], project_path.display().to_string());
        assert_eq!(body["remotes"][0]["name"], "example");
        assert_eq!(
            body["remotes"][0]["fields"],
            serde_json::json!(["signing_key", "principal_id", "vault_fingerprint"])
        );
        assert_eq!(
            body["cures"][0],
            "ryeos remote configure example --url <https-url> --expect-fingerprint <fingerprint>"
        );
        assert!(format!("{err:#}").contains("ryeos remote configure"));

        // The by-name layered map refuses too, so no caller can fall back
        // to the project's key.
        assert!(load_remotes_layered(app_root.path(), Some(&project_root_path)).is_err());
        // The node's own pin is untouched.
        let node_only = load_remotes_layered(app_root.path(), None).unwrap();
        assert_eq!(node_only["example"].signing_key, test_signing_key(3));
    }

    #[test]
    fn project_remote_cannot_supply_a_new_pinned_remote() {
        let app_root = tempfile::tempdir().unwrap();
        let project_root = tempfile::tempdir().unwrap();
        write_project_remotes(
            project_root.path(),
            &format!(
                r#"
remotes:
  project-only:
    name: project-only
    url: https://project-only.example.com
    signing_key: {}
"#,
                test_signing_key(8),
            ),
        );
        let err = load_remotes_layered_report(app_root.path(), Some(project_root.path()))
            .expect_err("a project may not supply a remote's trust material");
        let (code, _, body) = structured_refusal(&err);
        assert_eq!(code, PROJECT_REMOTE_TRUST_MATERIAL);
        assert_eq!(body["remotes"][0]["name"], "project-only");
        assert_eq!(
            body["remotes"][0]["fields"],
            serde_json::json!(["signing_key"])
        );
    }

    #[test]
    fn project_remote_naming_a_descriptor_or_fingerprint_is_refused() {
        let app_root = tempfile::tempdir().unwrap();
        let project_root = tempfile::tempdir().unwrap();
        write_project_remotes(
            project_root.path(),
            &format!(
                r#"
remotes:
  pinned:
    descriptor: /tmp/remote.yaml
  fingerprinted:
    fingerprint: {}
"#,
                test_fingerprint(9),
            ),
        );
        let err = load_remotes_layered_report(app_root.path(), Some(project_root.path()))
            .expect_err("descriptor and fingerprint are trust material");
        let (_, _, body) = structured_refusal(&err);
        assert_eq!(
            body["remotes"],
            serde_json::json!([
                { "name": "fingerprinted", "fields": ["fingerprint"] },
                { "name": "pinned", "fields": ["descriptor"] },
            ])
        );
    }

    #[test]
    fn project_remote_without_trust_fields_is_reported_not_loaded() {
        let app_root = tempfile::tempdir().unwrap();
        let project_root = tempfile::tempdir().unwrap();
        let mut node = HashMap::new();
        node.insert("default".to_string(), node_remote("default", 2));
        save_remotes(app_root.path(), &node).unwrap();
        write_project_remotes(
            project_root.path(),
            r#"
remotes:
  default:
    name: default
    url: https://project-level.example.com
    site_id: site:project
    ingest_ignore:
      patterns: []
"#,
        );

        let report =
            load_remotes_layered_report(app_root.path(), Some(project_root.path())).unwrap();
        let loaded = &report.remotes["default"];
        assert_eq!(loaded.scope, RemoteConfigScope::Operator);
        assert_eq!(loaded.config.url, "https://default.node.example.com");
        assert_eq!(report.invalid.len(), 1);
        assert_eq!(report.invalid[0].name, "default");
        assert!(matches!(
            report.invalid[0].scope,
            RemoteConfigScope::Project { .. }
        ));
        assert!(report.invalid[0].error.contains("pinned node key"));
        assert!(
            report.invalid[0]
                .repair_hint
                .contains("ryeos remote configure default")
        );
    }

    #[test]
    fn node_root_as_project_reads_the_node_file_once() {
        let app_root = tempfile::tempdir().unwrap();
        let app_root_path = app_root.path().canonicalize().unwrap();
        let mut node = HashMap::new();
        node.insert("example".to_string(), node_remote("example", 3));
        save_remotes(&app_root_path, &node).unwrap();

        let report = load_remotes_layered_report(&app_root_path, Some(&app_root_path)).unwrap();
        assert_eq!(report.remotes["example"].scope, RemoteConfigScope::Operator);
        assert!(report.invalid.is_empty());
    }

    #[test]
    fn retained_project_remote_cannot_override_node_signing_key() {
        let app_root = tempfile::tempdir().unwrap();
        let project_root = tempfile::tempdir().unwrap();
        let mut node = HashMap::new();
        node.insert("example".to_string(), node_remote("example", 3));
        save_remotes(app_root.path(), &node).unwrap();
        write_project_remotes(
            project_root.path(),
            &format!(
                "remotes:\n  example:\n    signing_key: {}\n",
                test_signing_key(4)
            ),
        );
        let project = lillux::PinnedDirectory::open(project_root.path())
            .unwrap()
            .expect("project directory exists");

        let err =
            load_remotes_layered_report_under_project_authority(app_root.path(), Some(&project))
                .expect_err("the retained project layer refuses trust material too");
        let (code, _, _) = structured_refusal(&err);
        assert_eq!(code, PROJECT_REMOTE_TRUST_MATERIAL);
    }

    #[test]
    fn typed_load_error_keeps_the_project_refusal() {
        let err = project_remote_trust_refusal(
            Path::new("/p/.ai/config/remotes/remotes.yaml"),
            &[("example".to_string(), vec!["signing_key"])],
        );
        match typed_load_error_or(err, |e| HandlerError::Internal(format!("{e:#}"))) {
            HandlerError::Structured { code, .. } => {
                assert_eq!(code, PROJECT_REMOTE_TRUST_MATERIAL)
            }
            other => panic!("typed refusal was flattened: {other:?}"),
        }
        let plain = typed_load_error_or(anyhow::anyhow!("disk"), |e| {
            HandlerError::Internal(format!("{e:#}"))
        });
        assert!(matches!(plain, HandlerError::Internal(ref m) if m == "disk"));
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
                ingest_ignore: Some(ryeos_app::ignore::IgnoreConfig { patterns: vec![] }),
                project_bindings: HashMap::new(),
            },
        );
        let err = save_remotes(tmpdir.path(), &remotes).unwrap_err();
        assert!(err.to_string().contains("key must not be empty"));
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
            ingest_ignore: Some(ryeos_app::ignore::IgnoreConfig { patterns: vec![] }),
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
}
