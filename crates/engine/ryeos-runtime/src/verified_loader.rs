use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::Engine;
use lillux::crypto::VerifyingKey;
use ryeos_engine::trust::{
    MAX_TRUST_DIRECTORY_BYTES, MAX_TRUST_DOCUMENTS, MAX_TRUST_DOCUMENT_BYTES,
    MAX_TRUST_TRAVERSAL_DEPTH, MAX_TRUST_TRAVERSAL_ENTRIES,
};
use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;

const MAX_CONFIG_SOURCE_BYTES: u64 = 1024 * 1024;

/// Strict, three-state error returned by [`VerifiedLoader::load_config_strict`].
///
/// Distinguishes "candidate file is absent" (`Ok(None)`) from "candidate
/// file exists but is broken" (`Err(_)`). Each variant carries the
/// offending file path so operators can act without re-running with
/// debug logs.
#[derive(Debug, Error)]
pub enum ConfigLoadError {
    #[error("config verify failed at {}: {source}", path.display())]
    VerifyFailed {
        path: PathBuf,
        #[source]
        source: anyhow::Error,
    },
    #[error("config raw-YAML parse failed at {}: {source}", path.display())]
    RawYamlParseFailed {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("config typed-parse failed at {}: {source}", path.display())]
    TypedParseFailed {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
}

impl ConfigLoadError {
    /// File path of the candidate that triggered the error. Useful for
    /// callers wrapping the error in higher-level diagnostics.
    pub fn path(&self) -> &Path {
        match self {
            Self::VerifyFailed { path, .. }
            | Self::RawYamlParseFailed { path, .. }
            | Self::TypedParseFailed { path, .. } => path,
        }
    }
}

/// Strictness policy for config/item verification.
///
/// `Permissive` is the historical default — accepts unsigned files
/// and unknown-signer files with a warning. Suitable for development
/// where bundle-signing may lag.
///
/// `Required` rejects unsigned and unknown-signer files outright.
/// Used for security-sensitive configs where a wrong source means
/// vault secrets get redirected (provider configs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadStrictness {
    Permissive,
    Required,
}

#[derive(Debug, Clone)]
pub struct TrustedKey {
    pub fingerprint: String,
    pub verifying_key: VerifyingKey,
    pub owner: String,
}

#[derive(Debug, Clone)]
pub struct TrustStore {
    keys: HashMap<String, TrustedKey>,
    project_sources: Vec<TrustSourceDependency>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct TrustSourceDependency {
    logical_path: PathBuf,
    source_hash: String,
}

impl TrustStore {
    /// Load trust from the project's trusted-keys dir plus the
    /// node's trusted-keys dir (`<app_root>/.ai/config/keys/trusted`,
    /// passed explicitly by the caller — the daemon for preflight, the
    /// launch envelope for runtimes). Bundle roots are NOT a trust
    /// authority: a bundle cannot ship keys that vouch for itself.
    pub fn load(project_root: &Path, node_trusted_keys_dir: &Path) -> Result<Self> {
        Self::load_with_optional_project(Some(project_root), node_trusted_keys_dir)
    }

    fn load_with_optional_project(
        project_root: Option<&Path>,
        node_trusted_keys_dir: &Path,
    ) -> Result<Self> {
        let mut keys = HashMap::new();

        let project_trusted_dir = project_root.map(|root| root.join(".ai/config/keys/trusted"));
        let mut project_sources = Vec::new();
        for (dir, is_project) in project_trusted_dir
            .as_deref()
            .map(|dir| (dir, true))
            .into_iter()
            .chain(std::iter::once((node_trusted_keys_dir, false)))
        {
            let mut file_count = 0_usize;
            let mut total_bytes = 0_u64;
            lillux::visit_regular_files_no_follow_bounded(
                dir,
                lillux::DirectoryTraversalBudget::new(
                    MAX_TRUST_TRAVERSAL_ENTRIES,
                    MAX_TRUST_TRAVERSAL_DEPTH,
                ),
                |_relative, is_directory| Ok(is_directory),
                |relative, file| {
                    file_count = file_count.saturating_add(1);
                    if file_count > MAX_TRUST_DOCUMENTS {
                        bail!(
                            "trust directory {} exceeds {MAX_TRUST_DOCUMENTS} regular files",
                            dir.display()
                        );
                    }
                    if relative.extension().and_then(|extension| extension.to_str()) != Some("toml")
                    {
                        return Ok(());
                    }
                    let bytes =
                        lillux::read_open_regular_file_bounded(file, MAX_TRUST_DOCUMENT_BYTES)?;
                    total_bytes = total_bytes
                        .checked_add(bytes.len() as u64)
                        .ok_or_else(|| anyhow::anyhow!("trust directory byte count overflow"))?;
                    if total_bytes > MAX_TRUST_DIRECTORY_BYTES {
                        bail!(
                            "trust directory {} exceeds {MAX_TRUST_DIRECTORY_BYTES} aggregate bytes",
                            dir.display()
                        );
                    }
                    let path = dir.join(relative);
                    let content = std::str::from_utf8(&bytes).with_context(|| {
                        format!("trusted key document is not UTF-8: {}", path.display())
                    })?;
                    let key = Self::parse_trusted_key_toml_content(&path, content)?;
                    if is_project {
                        project_sources.push(TrustSourceDependency {
                            logical_path: relative.to_path_buf(),
                            source_hash: lillux::sha256_hex(&bytes),
                        });
                    }
                    tracing::info!(
                        fingerprint = %key.fingerprint,
                        owner = %key.owner,
                        "loaded trusted key"
                    );
                    keys.entry(key.fingerprint.clone()).or_insert(key);
                    Ok(())
                },
            )
            .with_context(|| format!("load trusted keys from {}", dir.display()))?;
        }

        if !keys.is_empty() {
            tracing::info!(count = keys.len(), "trust store loaded");
        }

        project_sources.sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
        Ok(Self {
            keys,
            project_sources,
        })
    }

    fn load_with_project_content(
        project_content: &dyn ryeos_engine::project_content::AuthoritativeProjectContent,
        node_trusted_keys_dir: &Path,
    ) -> Result<Self> {
        let mut node = Self::load_with_optional_project(None, node_trusted_keys_dir)?;
        let prefix = Path::new(".ai/config/keys/trusted");
        let entries = project_content.list_files(prefix, false, MAX_TRUST_DOCUMENTS)?;
        let mut total_bytes = 0_u64;
        let mut project_sources = Vec::new();
        for entry in entries.into_iter().filter(|entry| {
            entry
                .relative_path
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("toml")
        }) {
            if entry.size > MAX_TRUST_DOCUMENT_BYTES {
                bail!(
                    "project trust document {} exceeds {MAX_TRUST_DOCUMENT_BYTES} bytes",
                    entry.relative_path.display()
                );
            }
            total_bytes = total_bytes
                .checked_add(entry.size)
                .ok_or_else(|| anyhow::anyhow!("project trust byte count overflow"))?;
            if total_bytes > MAX_TRUST_DIRECTORY_BYTES {
                bail!(
                    "project trust directory exceeds {MAX_TRUST_DIRECTORY_BYTES} aggregate bytes"
                );
            }
            let logical_path = prefix.join(&entry.relative_path);
            let bytes = project_content
                .read_file(&logical_path, MAX_TRUST_DOCUMENT_BYTES)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "admitted project trust document disappeared: {}",
                        logical_path.display()
                    )
                })?;
            let content = std::str::from_utf8(&bytes).with_context(|| {
                format!(
                    "admitted project trust document is not UTF-8: {}",
                    logical_path.display()
                )
            })?;
            let source_hash = lillux::sha256_hex(&bytes);
            let key = Self::parse_trusted_key_toml_content(&logical_path, content)?;
            node.keys.insert(key.fingerprint.clone(), key);
            project_sources.push(TrustSourceDependency {
                logical_path: entry.relative_path,
                source_hash,
            });
        }
        project_sources.sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
        node.project_sources = project_sources;
        Ok(node)
    }

    fn parse_trusted_key_toml_content(path: &Path, content: &str) -> Result<TrustedKey> {
        let mut fingerprint = None;
        let mut owner = String::new();
        let mut pem_lines: Vec<String> = Vec::new();
        let mut in_pem = false;
        // Single-line `pem = "ed25519:<b64>"` form, written by the
        // daemon's self-trust bootstrap and surfaced via the `ryeos-cli`
        // `identity-public-key` verb. The multi-line
        // `-----BEGIN PUBLIC KEY-----` PEM form is also supported (see
        // the daemon's trusted-signer fixture in
        // `crates/bin/daemon/tests/fixtures/trusted_signers/`). Either form is
        // accepted; if both appear the multi-line PEM wins (it is the
        // strictly typed format).
        let mut inline_key_b64: Option<String> = None;

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            if trimmed.starts_with("-----BEGIN PUBLIC KEY-----") {
                in_pem = true;
                continue;
            }
            if trimmed.starts_with("-----END PUBLIC KEY-----") {
                in_pem = false;
                continue;
            }
            if in_pem {
                pem_lines.push(trimmed.to_string());
                continue;
            }
            if let Some(val) = trimmed.strip_prefix("fingerprint") {
                let val = val.trim_start_matches(['=', ' ']).trim().trim_matches('"');
                fingerprint = Some(val.to_string());
            }
            if let Some(val) = trimmed.strip_prefix("owner") {
                let val = val.trim_start_matches(['=', ' ']).trim().trim_matches('"');
                owner = val.to_string();
            }
            // Accept both `pem` and `public_key` as field names for the
            // inline ed25519 key.  `PUBLISHER_TRUST.toml` files use
            // `public_key`; the daemon's self-trust bootstrap uses `pem`.
            let maybe_key = trimmed
                .strip_prefix("pem")
                .or_else(|| trimmed.strip_prefix("public_key"));
            if let Some(val) = maybe_key {
                let val = val.trim_start_matches(['=', ' ']).trim().trim_matches('"');
                if let Some(b64) = val.strip_prefix("ed25519:") {
                    inline_key_b64 = Some(b64.to_string());
                }
            }
        }

        let fingerprint = fingerprint.ok_or_else(|| anyhow::anyhow!("missing fingerprint"))?;
        let key_bytes: [u8; 32] = if !pem_lines.is_empty() {
            let pem_b64: String = pem_lines.join("");
            let pem_bytes = base64::engine::general_purpose::STANDARD
                .decode(&pem_b64)
                .context("invalid base64 in PEM")?;
            if pem_bytes.len() < 44 {
                bail!("PEM too short for Ed25519 public key");
            }
            pem_bytes[pem_bytes.len() - 32..]
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid key length"))?
        } else if let Some(b64) = inline_key_b64 {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(&b64)
                .context("invalid base64 in inline ed25519 key")?;
            if raw.len() != 32 {
                bail!(
                    "inline ed25519 key has wrong length: {} (expected 32 raw bytes)",
                    raw.len()
                );
            }
            raw.try_into()
                .map_err(|_| anyhow::anyhow!("invalid inline ed25519 key length"))?
        } else {
            bail!(
                "trust entry at {} has no public-key block: expected either a multi-line \
                 `-----BEGIN PUBLIC KEY-----` PEM or a single-line `pem = \"ed25519:<base64>\"`",
                path.display()
            );
        };
        let verifying_key = VerifyingKey::from_bytes(&key_bytes)
            .map_err(|_| anyhow::anyhow!("invalid Ed25519 public key"))?;
        let observed_fingerprint = lillux::crypto::fingerprint(&verifying_key);
        if fingerprint != observed_fingerprint {
            bail!(
                "declared fingerprint {fingerprint} does not match public key fingerprint {observed_fingerprint}"
            );
        }

        Ok(TrustedKey {
            fingerprint,
            verifying_key,
            owner,
        })
    }

    pub fn get(&self, fingerprint: &str) -> Option<&TrustedKey> {
        self.keys.get(fingerprint)
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Stable identity of the exact trusted signer set used for verification.
    /// Include the decoded public key and owner as well as the declared
    /// fingerprint so malformed trust documents cannot alias an earlier
    /// generation by reusing a map key.
    pub fn identity(&self) -> String {
        let mut keys = self.keys.values().collect::<Vec<_>>();
        keys.sort_by(|left, right| left.fingerprint.cmp(&right.fingerprint));
        let mut identity = Vec::new();
        for key in keys {
            append_identity_field(&mut identity, key.fingerprint.as_bytes());
            append_identity_field(&mut identity, key.verifying_key.as_bytes());
            append_identity_field(&mut identity, key.owner.as_bytes());
        }
        lillux::sha256_hex(&identity)
    }
}

fn append_identity_field(identity: &mut Vec<u8>, field: &[u8]) {
    identity.extend_from_slice(&(field.len() as u64).to_be_bytes());
    identity.extend_from_slice(field);
}

pub struct VerifiedLoader {
    project_root: PathBuf,
    project_config_enabled: bool,
    node_config_root: Option<PathBuf>,
    bundle_roots: Vec<PathBuf>,
    node_trusted_keys_dir: PathBuf,
    effective_trust_store: TrustStore,
    node_trust_store: TrustStore,
}

#[derive(Debug)]
pub struct VerifiedContent {
    pub content: String,
    pub hash: String,
    pub source_hash: String,
    pub signer_fingerprint: Option<String>,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum ConfigCandidateState {
    Absent,
    Present {
        source_hash: String,
        content_hash: String,
        signer_fingerprint: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ConfigCandidateRootClass {
    Project,
    Node,
    Bundle,
}

#[derive(Debug, Clone)]
struct ConfigCandidateDependency {
    root_class: ConfigCandidateRootClass,
    root_index: usize,
    logical_path: PathBuf,
    path: PathBuf,
    state: ConfigCandidateState,
}

/// Complete positive and negative dependency proof for one merged config.
///
/// Absolute paths are retained only for live revalidation. The stable digest
/// commits to ordered root class/index and content state, so disposable pinned
/// materialization paths never become config identity.
#[derive(Debug, Clone)]
pub struct ConfigDependencyProof {
    config_id: String,
    effective_trust_identity: String,
    node_trust_identity: String,
    project_trust_sources: Vec<TrustSourceDependency>,
    candidates: Vec<ConfigCandidateDependency>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigDependencyProofStatus {
    Current,
    MutableAuthorityChanged,
    ImmutableAuthorityMismatch,
}

impl ConfigDependencyProof {
    pub fn trust_identities_match(
        &self,
        effective_identity: Option<&str>,
        node_identity: &str,
    ) -> bool {
        self.node_trust_identity == node_identity
            && effective_identity
                .map(|identity| self.effective_trust_identity == identity)
                .unwrap_or(true)
    }

    pub fn identity_digest(&self) -> Result<String> {
        #[derive(Serialize)]
        struct StableCandidate<'a> {
            root_class: ConfigCandidateRootClass,
            root_index: usize,
            logical_path: &'a Path,
            state: &'a ConfigCandidateState,
        }
        #[derive(Serialize)]
        struct StableProof<'a> {
            schema_version: u32,
            config_id: &'a str,
            effective_trust_identity: &'a str,
            node_trust_identity: &'a str,
            project_trust_sources: &'a [TrustSourceDependency],
            candidates: Vec<StableCandidate<'a>>,
        }
        let value = serde_json::to_value(StableProof {
            schema_version: 1,
            config_id: &self.config_id,
            effective_trust_identity: &self.effective_trust_identity,
            node_trust_identity: &self.node_trust_identity,
            project_trust_sources: &self.project_trust_sources,
            candidates: self
                .candidates
                .iter()
                .map(|candidate| StableCandidate {
                    root_class: candidate.root_class,
                    root_index: candidate.root_index,
                    logical_path: &candidate.logical_path,
                    state: &candidate.state,
                })
                .collect(),
        })?;
        let canonical = lillux::canonical_json(&value)?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }

    /// Re-prove every positive and negative dependency against the same roots.
    /// This is required for live cache hits; exact pinned and sealed COW
    /// callers instead bind the proof to admitted content authority.
    pub fn revalidate_current(&self) -> bool {
        self.revalidate_candidates(|candidate| Some(candidate.path.clone()))
    }

    /// Re-prove this snapshot against a new materialization of the same
    /// logical project/bundle authority. This is the COW/pinned cache-hit
    /// path: disposable checkout paths never become part of the proof.
    pub fn revalidate_against(
        &self,
        project_root: Option<&Path>,
        node_config_root: Option<&Path>,
        bundle_roots: &[PathBuf],
    ) -> bool {
        self.revalidate_candidates(|candidate| match candidate.root_class {
            ConfigCandidateRootClass::Project if candidate.root_index == 0 => {
                project_root.map(|root| root.join(&candidate.logical_path))
            }
            ConfigCandidateRootClass::Node if candidate.root_index == 0 => {
                node_config_root.map(|root| root.join(&candidate.logical_path))
            }
            ConfigCandidateRootClass::Bundle => bundle_roots
                .get(candidate.root_index)
                .map(|root| root.join(&candidate.logical_path)),
            _ => None,
        })
    }

    /// Revalidate only roots that remain mutable under the admitted execution
    /// authority. Bundle roots are sealed by the engine generation; an exact
    /// pinned project root is sealed by its opaque materialization proof. Node
    /// config is always mutable and therefore always re-probed.
    pub fn revalidate_mutable_against(
        &self,
        project_root: Option<&Path>,
        node_config_root: Option<&Path>,
        revalidate_project: bool,
    ) -> bool {
        self.candidates
            .iter()
            .all(|candidate| match candidate.root_class {
                ConfigCandidateRootClass::Bundle => true,
                ConfigCandidateRootClass::Project if !revalidate_project => true,
                ConfigCandidateRootClass::Project if candidate.root_index == 0 => project_root
                    .map(|root| root.join(&candidate.logical_path))
                    .is_some_and(|path| revalidate_candidate_at(candidate, &path)),
                ConfigCandidateRootClass::Node if candidate.root_index == 0 => node_config_root
                    .map(|root| root.join(&candidate.logical_path))
                    .is_some_and(|path| revalidate_candidate_at(candidate, &path)),
                _ => false,
            })
    }

    /// Revalidate project inputs against the exact admitted project-content
    /// closure. The verified loader owns config/trust discovery policy; this
    /// method only asks the generic authority to prove the resulting positive
    /// and negative dependencies. Node config and node trust remain mutable
    /// node-local authority and are checked separately by the caller.
    pub fn revalidate_under_project_authority(
        &self,
        project_root: Option<&Path>,
        node_config_root: Option<&Path>,
        project_content: Option<&dyn ryeos_engine::project_content::AuthoritativeProjectContent>,
    ) -> bool {
        self.revalidate_under_project_authority_status(
            project_root,
            node_config_root,
            project_content,
        ) == ConfigDependencyProofStatus::Current
    }

    /// Classify exact-project contradictions separately from mutable node
    /// configuration changes. Callers may retry the latter under a bound; they
    /// must fail closed immediately when admitted project content disagrees.
    pub fn revalidate_under_project_authority_status(
        &self,
        project_root: Option<&Path>,
        node_config_root: Option<&Path>,
        project_content: Option<&dyn ryeos_engine::project_content::AuthoritativeProjectContent>,
    ) -> ConfigDependencyProofStatus {
        let trust_prefix = Path::new(".ai/config/keys/trusted");
        let trust_matches = match project_content {
            Some(content) => content
                .list_files(trust_prefix, false, MAX_TRUST_DOCUMENTS)
                .map(|entries| {
                    let mut observed = entries
                        .into_iter()
                        .filter(|entry| {
                            entry
                                .relative_path
                                .extension()
                                .and_then(|value| value.to_str())
                                == Some("toml")
                        })
                        .map(|entry| TrustSourceDependency {
                            logical_path: entry.relative_path,
                            source_hash: entry.content_hash,
                        })
                        .collect::<Vec<_>>();
                    observed.sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
                    observed == self.project_trust_sources
                })
                .unwrap_or(false),
            None => self.project_trust_sources.is_empty(),
        };
        if !trust_matches {
            return ConfigDependencyProofStatus::ImmutableAuthorityMismatch;
        }
        let mut status = ConfigDependencyProofStatus::Current;
        for candidate in &self.candidates {
            let candidate_status = match candidate.root_class {
                ConfigCandidateRootClass::Bundle => ConfigDependencyProofStatus::Current,
                ConfigCandidateRootClass::Node if candidate.root_index == 0 => {
                    if node_config_root
                        .map(|root| root.join(&candidate.logical_path))
                        .is_some_and(|path| revalidate_candidate_at(candidate, &path))
                    {
                        ConfigDependencyProofStatus::Current
                    } else {
                        ConfigDependencyProofStatus::MutableAuthorityChanged
                    }
                }
                ConfigCandidateRootClass::Project if candidate.root_index == 0 => {
                    let (Some(_project_root), Some(content)) = (project_root, project_content)
                    else {
                        return ConfigDependencyProofStatus::ImmutableAuthorityMismatch;
                    };
                    let relative = candidate.logical_path.clone();
                    let matches = match &candidate.state {
                        ConfigCandidateState::Absent => {
                            content.validates_absence(&relative).unwrap_or(false)
                        }
                        ConfigCandidateState::Present { source_hash, .. } => content
                            .validates_file(&relative, source_hash)
                            .unwrap_or(false),
                    };
                    if matches {
                        ConfigDependencyProofStatus::Current
                    } else {
                        ConfigDependencyProofStatus::ImmutableAuthorityMismatch
                    }
                }
                _ => ConfigDependencyProofStatus::ImmutableAuthorityMismatch,
            };
            status = combine_config_proof_status(status, candidate_status);
            if status == ConfigDependencyProofStatus::ImmutableAuthorityMismatch {
                return status;
            }
        }
        status
    }

    fn revalidate_candidates(
        &self,
        mut current_path: impl FnMut(&ConfigCandidateDependency) -> Option<PathBuf>,
    ) -> bool {
        self.candidates.iter().all(|candidate| {
            let Some(path) = current_path(candidate) else {
                return false;
            };
            match (
                &candidate.state,
                lillux::read_optional_regular_file_bounded_no_follow(
                    &path,
                    MAX_CONFIG_SOURCE_BYTES,
                ),
            ) {
                (ConfigCandidateState::Absent, Ok(None)) => true,
                (ConfigCandidateState::Present { source_hash, .. }, Ok(Some(current_source))) => {
                    lillux::sha256_hex(&current_source) == *source_hash
                }
                _ => false,
            }
        })
    }

    pub fn estimated_bytes(&self) -> usize {
        self.config_id
            .capacity()
            .saturating_add(self.effective_trust_identity.capacity())
            .saturating_add(self.node_trust_identity.capacity())
            .saturating_add(
                self.project_trust_sources
                    .iter()
                    .fold(0usize, |total, dependency| {
                        total
                            .saturating_add(
                                dependency.logical_path.as_os_str().as_encoded_bytes().len(),
                            )
                            .saturating_add(dependency.source_hash.capacity())
                    }),
            )
            .saturating_add(
                self.candidates
                    .capacity()
                    .saturating_mul(std::mem::size_of::<ConfigCandidateDependency>()),
            )
            .saturating_add(self.candidates.iter().fold(0usize, |total, candidate| {
                total
                    .saturating_add(candidate.logical_path.as_os_str().as_encoded_bytes().len())
                    .saturating_add(candidate.path.as_os_str().as_encoded_bytes().len())
                    .saturating_add(
                        serde_json::to_vec(&candidate.state)
                            .map(|serialized| serialized.len())
                            .unwrap_or(usize::MAX),
                    )
            }))
    }
}

fn combine_config_proof_status(
    left: ConfigDependencyProofStatus,
    right: ConfigDependencyProofStatus,
) -> ConfigDependencyProofStatus {
    use ConfigDependencyProofStatus::{
        Current, ImmutableAuthorityMismatch, MutableAuthorityChanged,
    };
    match (left, right) {
        (ImmutableAuthorityMismatch, _) | (_, ImmutableAuthorityMismatch) => {
            ImmutableAuthorityMismatch
        }
        (MutableAuthorityChanged, _) | (_, MutableAuthorityChanged) => MutableAuthorityChanged,
        (Current, Current) => Current,
    }
}

fn revalidate_candidate_at(candidate: &ConfigCandidateDependency, path: &Path) -> bool {
    match (
        &candidate.state,
        lillux::read_optional_regular_file_bounded_no_follow(path, MAX_CONFIG_SOURCE_BYTES),
    ) {
        (ConfigCandidateState::Absent, Ok(None)) => true,
        (ConfigCandidateState::Present { source_hash, .. }, Ok(Some(current_source))) => {
            lillux::sha256_hex(&current_source) == *source_hash
        }
        _ => false,
    }
}

#[derive(Debug)]
pub struct VerifiedConfigSnapshot<T> {
    pub value: T,
    pub dependency_proof: ConfigDependencyProof,
}

#[derive(Debug)]
pub struct VerifiedOptionalConfigSnapshot<T> {
    pub value: Option<T>,
    pub dependency_proof: ConfigDependencyProof,
}

#[derive(Debug)]
pub struct ScannedItem {
    pub name: String,
    pub path: PathBuf,
    pub root: PathBuf,
}

impl VerifiedLoader {
    fn trust_store_for_config_root(&self, root_class: ConfigCandidateRootClass) -> &TrustStore {
        match root_class {
            ConfigCandidateRootClass::Project => &self.effective_trust_store,
            ConfigCandidateRootClass::Node | ConfigCandidateRootClass::Bundle => {
                &self.node_trust_store
            }
        }
    }

    /// `bundle_roots` are CONFIG search roots only (configs ship in
    /// bundles); trust comes exclusively from the project root and the
    /// explicit node trusted-keys dir. No hidden env reads here —
    /// the caller owns the trust context.
    pub fn new(
        project_root: PathBuf,
        bundle_roots: Vec<PathBuf>,
        node_trusted_keys_dir: &Path,
    ) -> Result<Self> {
        Self::new_with_node_config(project_root, None, bundle_roots, node_trusted_keys_dir)
    }

    /// Construct with an explicit node-local config layer. Config merges in
    /// bundle → node → project order, while signer trust remains sourced only
    /// from the project and node trusted-key directories.
    pub fn new_with_node_config(
        project_root: PathBuf,
        node_config_root: Option<PathBuf>,
        bundle_roots: Vec<PathBuf>,
        node_trusted_keys_dir: &Path,
    ) -> Result<Self> {
        let effective_trust_store = TrustStore::load(&project_root, node_trusted_keys_dir)?;
        let node_trust_store = TrustStore::load_with_optional_project(None, node_trusted_keys_dir)?;
        Ok(Self {
            project_root,
            project_config_enabled: true,
            node_config_root,
            bundle_roots,
            node_trusted_keys_dir: node_trusted_keys_dir.to_path_buf(),
            effective_trust_store,
            node_trust_store,
        })
    }

    pub fn new_with_node_config_under_project_authority(
        project_root: PathBuf,
        project_content: &dyn ryeos_engine::project_content::AuthoritativeProjectContent,
        node_config_root: Option<PathBuf>,
        bundle_roots: Vec<PathBuf>,
        node_trusted_keys_dir: &Path,
    ) -> Result<Self> {
        let effective_trust_store =
            TrustStore::load_with_project_content(project_content, node_trusted_keys_dir)?;
        let node_trust_store = TrustStore::load_with_optional_project(None, node_trusted_keys_dir)?;
        Ok(Self {
            project_root,
            project_config_enabled: true,
            node_config_root,
            bundle_roots,
            node_trusted_keys_dir: node_trusted_keys_dir.to_path_buf(),
            effective_trust_store,
            node_trust_store,
        })
    }

    /// Construct for a projectless execution. Its scratch cwd is deliberately
    /// excluded from both config and trust resolution.
    pub fn new_projectless_with_node_config(
        node_config_root: Option<PathBuf>,
        bundle_roots: Vec<PathBuf>,
        node_trusted_keys_dir: &Path,
    ) -> Result<Self> {
        let node_trust_store = TrustStore::load_with_optional_project(None, node_trusted_keys_dir)?;
        Ok(Self {
            project_root: node_config_root
                .clone()
                .unwrap_or_else(|| node_trusted_keys_dir.to_path_buf()),
            project_config_enabled: false,
            node_config_root,
            bundle_roots,
            node_trusted_keys_dir: node_trusted_keys_dir.to_path_buf(),
            effective_trust_store: node_trust_store.clone(),
            node_trust_store,
        })
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn node_trusted_keys_dir(&self) -> &Path {
        &self.node_trusted_keys_dir
    }

    pub fn trust_store(&self) -> &TrustStore {
        &self.effective_trust_store
    }

    /// Identity of both source-specific trust domains used by merged config:
    /// effective project trust for project candidates, and node trust for
    /// node/bundle candidates.
    pub fn config_trust_identity(&self) -> String {
        let mut identity = Vec::new();
        append_identity_field(
            &mut identity,
            self.effective_trust_store.identity().as_bytes(),
        );
        append_identity_field(&mut identity, self.node_trust_store.identity().as_bytes());
        lillux::sha256_hex(&identity)
    }

    pub fn effective_trust_identity(&self) -> String {
        self.effective_trust_store.identity()
    }

    pub fn node_trust_identity(&self) -> String {
        self.node_trust_store.identity()
    }

    pub fn node_trust_identity_from(node_trusted_keys_dir: &Path) -> Result<String> {
        Ok(TrustStore::load_with_optional_project(None, node_trusted_keys_dir)?.identity())
    }

    fn kind_subdir(kind: &str) -> &'static str {
        match kind {
            "directive" => ".ai/directives/",
            "tool" => ".ai/tools/",
            "knowledge" => ".ai/knowledge/",
            "config" => ".ai/config/",
            _ => ".ai/",
        }
    }

    /// Load and verify a file. Permissive mode — warns on unsigned/unknown-signer.
    pub fn load_verified(&self, kind: &str, path: &Path) -> Result<VerifiedContent> {
        self.load_verified_with_strictness(kind, path, LoadStrictness::Permissive)
    }

    /// Load and verify a file with configurable strictness.
    pub fn load_verified_with_strictness(
        &self,
        kind: &str,
        path: &Path,
        strictness: LoadStrictness,
    ) -> Result<VerifiedContent> {
        self.load_verified_with_trust_store(kind, path, strictness, &self.effective_trust_store)
    }

    fn load_verified_with_trust_store(
        &self,
        kind: &str,
        path: &Path,
        strictness: LoadStrictness,
        trust_store: &TrustStore,
    ) -> Result<VerifiedContent> {
        let raw = String::from_utf8(
            lillux::read_regular_file_bounded_no_follow(path, MAX_CONFIG_SOURCE_BYTES)
                .with_context(|| format!("securely reading {}", path.display()))?,
        )
        .with_context(|| format!("securely reading {}", path.display()))?;
        self.load_verified_content_with_trust_store(kind, path, raw, strictness, trust_store)
    }

    fn load_verified_content_with_trust_store(
        &self,
        kind: &str,
        path: &Path,
        raw: String,
        strictness: LoadStrictness,
        trust_store: &TrustStore,
    ) -> Result<VerifiedContent> {
        let source_hash = lillux::sha256_hex(raw.as_bytes());

        let content = lillux::signature::strip_signature_lines(&raw);

        let hash = lillux::sha256_hex(content.as_bytes());
        tracing::trace!(path = %path.display(), hash = %hash, "computed content hash for verification");

        let (prefix, suffix) = Self::signature_format_for(kind);
        let verified = if let Some(sig_header) = Self::parse_first_signature(&raw, prefix, suffix) {
            if sig_header.content_hash != hash {
                bail!(
                    "content hash mismatch in {}: signature says {}, computed {}",
                    path.display(),
                    sig_header.content_hash,
                    hash
                );
            }

            if let Some(trusted_key) = trust_store.get(&sig_header.signer_fingerprint) {
                if !lillux::signature::verify_signature(
                    &sig_header.content_hash,
                    &sig_header.signature_b64,
                    &trusted_key.verifying_key,
                ) {
                    bail!(
                        "signature verification failed for {} (signer: {})",
                        path.display(),
                        sig_header.signer_fingerprint
                    );
                }
                VerifiedContent {
                    content,
                    hash,
                    source_hash,
                    signer_fingerprint: Some(sig_header.signer_fingerprint),
                    path: path.to_path_buf(),
                }
            } else {
                match strictness {
                    LoadStrictness::Permissive => {
                        tracing::warn!(
                            "signed by unknown signer {} — not in trust store: {}",
                            sig_header.signer_fingerprint,
                            path.display()
                        );
                        VerifiedContent {
                            content,
                            hash,
                            source_hash,
                            signer_fingerprint: Some(sig_header.signer_fingerprint),
                            path: path.to_path_buf(),
                        }
                    }
                    LoadStrictness::Required => {
                        bail!(
                            "REJECTED: {} is signed by unknown signer {} \
                             (not in trust store). Strict mode requires a \
                             trusted publisher signature for this config kind.",
                            path.display(),
                            sig_header.signer_fingerprint
                        );
                    }
                }
            }
        } else {
            match strictness {
                LoadStrictness::Permissive => VerifiedContent {
                    content,
                    hash,
                    source_hash,
                    signer_fingerprint: None,
                    path: path.to_path_buf(),
                },
                LoadStrictness::Required => {
                    bail!(
                        "REJECTED: {} is unsigned. Strict mode requires a \
                         valid publisher signature for this config kind. \
                         Re-sign with: ./scripts/populate-bundles.sh \
                         --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev",
                        path.display()
                    );
                }
            }
        };

        Ok(verified)
    }

    fn signature_format_for(kind: &str) -> (&'static str, Option<&'static str>) {
        match kind {
            "directive" => ("<!--", Some("-->")),
            "knowledge" => ("<!--", Some("-->")),
            "tool" => ("#", None),
            "config" => ("#", None),
            _ => ("#", None),
        }
    }

    fn parse_first_signature(
        raw: &str,
        prefix: &str,
        suffix: Option<&str>,
    ) -> Option<lillux::signature::SignatureHeader> {
        for line in raw.lines().take(2) {
            if let Some(header) = lillux::signature::parse_signature_line(line, prefix, suffix) {
                return Some(header);
            }
            if prefix != "#" {
                if let Some(header) = lillux::signature::parse_signature_line(line, "#", None) {
                    return Some(header);
                }
            }
        }
        None
    }

    /// Strict, three-state config loader:
    ///
    /// - `Ok(None)`  — no candidate file exists at the expected path
    ///   under any space root. Truly absent.
    /// - `Ok(Some(_))` — a candidate exists, verified, and parsed
    ///   into the typed shape successfully.
    /// - `Err(_)` — a candidate file exists but verification or
    ///   parsing failed. The error names the file path and the
    ///   underlying cause so callers can surface a loud diagnostic.
    pub fn load_config_strict<T: DeserializeOwned>(
        &self,
        config_id: &str,
    ) -> std::result::Result<Option<T>, ConfigLoadError> {
        let subdir = Self::kind_subdir("config");
        let item_path = PathBuf::from(format!("{subdir}{config_id}.yaml"));

        // Collect least-specific first (bundles, node, then project), so the
        // deep merge below yields `project > node > bundle` precedence.
        let mut candidate_paths = Vec::new();

        for bundle_root in &self.bundle_roots {
            let p = bundle_root.join(&item_path);
            if p.exists() {
                candidate_paths.push((p, ConfigCandidateRootClass::Bundle));
            }
        }

        if let Some(node_config_root) = &self.node_config_root {
            let path = node_config_root.join(&item_path);
            if path.exists() {
                candidate_paths.push((path, ConfigCandidateRootClass::Node));
            }
        }

        if self.project_config_enabled {
            let path = self.project_root.join(&item_path);
            if path.exists() {
                candidate_paths.push((path, ConfigCandidateRootClass::Project));
            }
        }

        if candidate_paths.is_empty() {
            return Ok(None);
        }

        if candidate_paths.len() == 1 {
            let (path, root_class) = &candidate_paths[0];
            let verified = self
                .load_verified_with_trust_store(
                    "config",
                    path,
                    LoadStrictness::Permissive,
                    self.trust_store_for_config_root(*root_class),
                )
                .map_err(|e| ConfigLoadError::VerifyFailed {
                    path: path.clone(),
                    source: e,
                })?;
            // Parse raw YAML first, then type-convert — same as the
            // merged path. This ensures YAML syntax errors always surface
            // as RawYamlParseFailed and type-shape errors as TypedParseFailed,
            // giving consistent enum semantics regardless of candidate count.
            let raw_value: serde_yaml::Value =
                serde_yaml::from_str(&verified.content).map_err(|e| {
                    ConfigLoadError::RawYamlParseFailed {
                        path: path.clone(),
                        source: e,
                    }
                })?;
            let value = serde_yaml::from_value(raw_value).map_err(|e| {
                ConfigLoadError::TypedParseFailed {
                    path: path.clone(),
                    source: e,
                }
            })?;
            return Ok(Some(value));
        }

        let mut merged = serde_yaml::Value::Null;
        for (path, root_class) in &candidate_paths {
            let verified = self
                .load_verified_with_trust_store(
                    "config",
                    path,
                    LoadStrictness::Permissive,
                    self.trust_store_for_config_root(*root_class),
                )
                .map_err(|e| ConfigLoadError::VerifyFailed {
                    path: path.clone(),
                    source: e,
                })?;
            let value =
                serde_yaml::from_str::<serde_yaml::Value>(&verified.content).map_err(|e| {
                    ConfigLoadError::RawYamlParseFailed {
                        path: path.clone(),
                        source: e,
                    }
                })?;
            merged = deep_merge_yaml(merged, value);
        }

        // The merged value isn't tied to a single file; surface the
        // last contributing file in the typed-parse error path so
        // operators have *some* lead. This keeps the error variant
        // shape stable (file path + underlying error).
        let last_path = candidate_paths
            .last()
            .map(|(path, _)| path.clone())
            .unwrap_or_else(|| item_path.clone());
        let value =
            serde_yaml::from_value::<T>(merged).map_err(|e| ConfigLoadError::TypedParseFailed {
                path: last_path,
                source: e,
            })?;
        Ok(Some(value))
    }

    /// Load one explicitly owned config file through the same verification and
    /// typed-parse boundary as composed config loading. This is for configured
    /// roots whose precedence is modeled by their consumer rather than by the
    /// bundle/project config overlay (for example operator hook policy).
    pub fn load_config_file_strict<T: DeserializeOwned>(
        &self,
        path: &Path,
    ) -> std::result::Result<Option<T>, ConfigLoadError> {
        if !path.exists() {
            return Ok(None);
        }
        let verified =
            self.load_verified("config", path)
                .map_err(|source| ConfigLoadError::VerifyFailed {
                    path: path.to_path_buf(),
                    source,
                })?;
        let raw_value: serde_yaml::Value =
            serde_yaml::from_str(&verified.content).map_err(|source| {
                ConfigLoadError::RawYamlParseFailed {
                    path: path.to_path_buf(),
                    source,
                }
            })?;
        serde_yaml::from_value(raw_value)
            .map(Some)
            .map_err(|source| ConfigLoadError::TypedParseFailed {
                path: path.to_path_buf(),
                source,
            })
    }

    /// Same as `load_config_strict` but also returns the set of source root
    /// labels (`"project"` / `"bundle"`) that contributed a file
    /// to the result, in resolution order (bundle → project; project wins).
    /// Used for trust decisions where ANY contribution from an untrusted
    /// root must be detectable.
    ///
    /// Returns `Ok(None)` if no root contributed a file.
    /// Load a config requiring a valid signature from a trusted publisher.
    /// Returns only the parsed value (no contributor labels).
    /// Rejects unsigned and unknown-signer files outright.
    pub fn load_config_strict_signed<T: DeserializeOwned>(
        &self,
        config_id: &str,
    ) -> std::result::Result<Option<T>, ConfigLoadError> {
        self.load_config_with_strictness(config_id, LoadStrictness::Required)
            .map(|opt| opt.map(|(v, _contribs)| v))
    }

    /// Load a signed merged config together with the exact positive and
    /// negative root dependencies that selected it.
    ///
    /// Unlike a selected-value digest, this proof changes when a previously
    /// absent higher-precedence project file appears. It is therefore suitable
    /// as one leg of a content-addressed launch-cache key.
    pub fn load_config_strict_signed_with_proof<T: DeserializeOwned>(
        &self,
        config_id: &str,
    ) -> std::result::Result<Option<VerifiedConfigSnapshot<T>>, ConfigLoadError> {
        self.load_optional_config_strict_signed_with_proof_from_authority(config_id, None)
            .map(|snapshot| {
                snapshot.value.map(|value| VerifiedConfigSnapshot {
                    value,
                    dependency_proof: snapshot.dependency_proof,
                })
            })
    }

    pub fn load_config_strict_signed_with_proof_under_project_authority<T: DeserializeOwned>(
        &self,
        config_id: &str,
        project_content: &dyn ryeos_engine::project_content::AuthoritativeProjectContent,
    ) -> std::result::Result<Option<VerifiedConfigSnapshot<T>>, ConfigLoadError> {
        self.load_optional_config_strict_signed_with_proof_from_authority(
            config_id,
            Some(project_content),
        )
        .map(|snapshot| {
            snapshot.value.map(|value| VerifiedConfigSnapshot {
                value,
                dependency_proof: snapshot.dependency_proof,
            })
        })
    }

    /// Signed config resolution with a proof even when every candidate is
    /// absent. Absence is static authority too: a cache may reuse a default
    /// only while no higher-precedence contributor has appeared.
    pub fn load_optional_config_strict_signed_with_proof<T: DeserializeOwned>(
        &self,
        config_id: &str,
    ) -> std::result::Result<VerifiedOptionalConfigSnapshot<T>, ConfigLoadError> {
        self.load_optional_config_strict_signed_with_proof_from_authority(config_id, None)
    }

    pub fn load_optional_config_strict_signed_with_proof_under_project_authority<
        T: DeserializeOwned,
    >(
        &self,
        config_id: &str,
        project_content: &dyn ryeos_engine::project_content::AuthoritativeProjectContent,
    ) -> std::result::Result<VerifiedOptionalConfigSnapshot<T>, ConfigLoadError> {
        self.load_optional_config_strict_signed_with_proof_from_authority(
            config_id,
            Some(project_content),
        )
    }

    fn load_optional_config_strict_signed_with_proof_from_authority<T: DeserializeOwned>(
        &self,
        config_id: &str,
        project_content: Option<&dyn ryeos_engine::project_content::AuthoritativeProjectContent>,
    ) -> std::result::Result<VerifiedOptionalConfigSnapshot<T>, ConfigLoadError> {
        let item_path = PathBuf::from(format!("{}{config_id}.yaml", Self::kind_subdir("config")));
        let mut candidates = Vec::with_capacity(
            self.bundle_roots.len() + 1 + usize::from(self.node_config_root.is_some()),
        );
        let mut verified_present = Vec::new();

        for (root_index, bundle_root) in self.bundle_roots.iter().enumerate() {
            self.capture_config_candidate(
                bundle_root.join(&item_path),
                item_path.clone(),
                ConfigCandidateRootClass::Bundle,
                root_index,
                &mut candidates,
                &mut verified_present,
            )?;
        }
        if let Some(node_config_root) = &self.node_config_root {
            self.capture_config_candidate(
                node_config_root.join(&item_path),
                item_path.clone(),
                ConfigCandidateRootClass::Node,
                0,
                &mut candidates,
                &mut verified_present,
            )?;
        }
        if self.project_config_enabled {
            match project_content {
                Some(content) => self.capture_project_config_candidate(
                    self.project_root.join(&item_path),
                    item_path.clone(),
                    content,
                    &mut candidates,
                    &mut verified_present,
                )?,
                None => self.capture_config_candidate(
                    self.project_root.join(&item_path),
                    item_path.clone(),
                    ConfigCandidateRootClass::Project,
                    0,
                    &mut candidates,
                    &mut verified_present,
                )?,
            }
        }

        let dependency_proof = ConfigDependencyProof {
            config_id: config_id.to_string(),
            effective_trust_identity: self.effective_trust_identity(),
            node_trust_identity: self.node_trust_identity(),
            project_trust_sources: self.effective_trust_store.project_sources.clone(),
            candidates,
        };
        if !self.config_dependency_proof_is_current(&dependency_proof, project_content) {
            return Err(ConfigLoadError::VerifyFailed {
                path: item_path.clone(),
                source: anyhow::anyhow!(
                    "config candidates changed while the verified snapshot was being captured"
                ),
            });
        }
        if verified_present.is_empty() {
            return Ok(VerifiedOptionalConfigSnapshot {
                value: None,
                dependency_proof,
            });
        }

        let mut merged = serde_yaml::Value::Null;
        let mut last_path = item_path;
        for verified in verified_present {
            last_path = verified.path.clone();
            let value =
                serde_yaml::from_str::<serde_yaml::Value>(&verified.content).map_err(|source| {
                    ConfigLoadError::RawYamlParseFailed {
                        path: verified.path,
                        source,
                    }
                })?;
            merged = deep_merge_yaml(merged, value);
        }
        let value = serde_yaml::from_value::<T>(merged).map_err(|source| {
            ConfigLoadError::TypedParseFailed {
                path: last_path.clone(),
                source,
            }
        })?;
        if !self.config_dependency_proof_is_current(&dependency_proof, project_content) {
            return Err(ConfigLoadError::VerifyFailed {
                path: last_path,
                source: anyhow::anyhow!(
                    "config candidates changed while the verified snapshot was being captured"
                ),
            });
        }
        Ok(VerifiedOptionalConfigSnapshot {
            value: Some(value),
            dependency_proof,
        })
    }

    fn config_dependency_proof_is_current(
        &self,
        proof: &ConfigDependencyProof,
        project_content: Option<&dyn ryeos_engine::project_content::AuthoritativeProjectContent>,
    ) -> bool {
        match project_content {
            Some(content) => proof.revalidate_under_project_authority(
                Some(&self.project_root),
                self.node_config_root.as_deref(),
                Some(content),
            ),
            None => proof.revalidate_current(),
        }
    }

    fn capture_project_config_candidate(
        &self,
        path: PathBuf,
        logical_path: PathBuf,
        project_content: &dyn ryeos_engine::project_content::AuthoritativeProjectContent,
        candidates: &mut Vec<ConfigCandidateDependency>,
        verified_present: &mut Vec<VerifiedContent>,
    ) -> std::result::Result<(), ConfigLoadError> {
        let source = project_content
            .read_file(&logical_path, MAX_CONFIG_SOURCE_BYTES)
            .map_err(|source| ConfigLoadError::VerifyFailed {
                path: path.clone(),
                source: anyhow::anyhow!(source),
            })?;
        match source {
            Some(source) => {
                let raw =
                    String::from_utf8(source).map_err(|source| ConfigLoadError::VerifyFailed {
                        path: path.clone(),
                        source: anyhow::anyhow!(source),
                    })?;
                let verified = self
                    .load_verified_content_with_trust_store(
                        "config",
                        &path,
                        raw,
                        LoadStrictness::Required,
                        &self.effective_trust_store,
                    )
                    .map_err(|source| ConfigLoadError::VerifyFailed {
                        path: path.clone(),
                        source,
                    })?;
                candidates.push(ConfigCandidateDependency {
                    root_class: ConfigCandidateRootClass::Project,
                    root_index: 0,
                    logical_path,
                    path,
                    state: ConfigCandidateState::Present {
                        source_hash: verified.source_hash.clone(),
                        content_hash: verified.hash.clone(),
                        signer_fingerprint: verified.signer_fingerprint.clone(),
                    },
                });
                verified_present.push(verified);
            }
            None => candidates.push(ConfigCandidateDependency {
                root_class: ConfigCandidateRootClass::Project,
                root_index: 0,
                logical_path,
                path,
                state: ConfigCandidateState::Absent,
            }),
        }
        Ok(())
    }

    fn capture_config_candidate(
        &self,
        path: PathBuf,
        logical_path: PathBuf,
        root_class: ConfigCandidateRootClass,
        root_index: usize,
        candidates: &mut Vec<ConfigCandidateDependency>,
        verified_present: &mut Vec<VerifiedContent>,
    ) -> std::result::Result<(), ConfigLoadError> {
        match lillux::inspect_optional_entry_no_follow(&path) {
            Ok(Some(lillux::secure_fs::PinnedEntryType::Regular)) => {
                let verified = self
                    .load_verified_with_trust_store(
                        "config",
                        &path,
                        LoadStrictness::Required,
                        self.trust_store_for_config_root(root_class),
                    )
                    .map_err(|source| ConfigLoadError::VerifyFailed {
                        path: path.clone(),
                        source,
                    })?;
                candidates.push(ConfigCandidateDependency {
                    root_class,
                    root_index,
                    logical_path,
                    path,
                    state: ConfigCandidateState::Present {
                        source_hash: verified.source_hash.clone(),
                        content_hash: verified.hash.clone(),
                        signer_fingerprint: verified.signer_fingerprint.clone(),
                    },
                });
                verified_present.push(verified);
                Ok(())
            }
            Ok(None) => {
                candidates.push(ConfigCandidateDependency {
                    root_class,
                    root_index,
                    logical_path,
                    path,
                    state: ConfigCandidateState::Absent,
                });
                Ok(())
            }
            Ok(Some(_)) => Err(ConfigLoadError::VerifyFailed {
                path: path.clone(),
                source: anyhow::anyhow!(
                    "config candidate is not a regular non-symlink file: {}",
                    path.display()
                ),
            }),
            Err(error) => Err(ConfigLoadError::VerifyFailed {
                path: path.clone(),
                source: error.context(format!(
                    "securely inspect config candidate {}",
                    path.display()
                )),
            }),
        }
    }

    /// Permissive provenance loader — permissive.
    pub fn load_config_with_provenance<T: DeserializeOwned>(
        &self,
        config_id: &str,
    ) -> std::result::Result<Option<(T, Vec<String>)>, ConfigLoadError> {
        self.load_config_with_strictness(config_id, LoadStrictness::Permissive)
    }

    /// Core loader with configurable strictness. Returns the parsed value
    /// and the list of contributing root labels.
    pub fn load_config_with_strictness<T: DeserializeOwned>(
        &self,
        config_id: &str,
        strictness: LoadStrictness,
    ) -> std::result::Result<Option<(T, Vec<String>)>, ConfigLoadError> {
        self.load_config_with_strictness_from_roots(config_id, strictness, true)
    }

    /// Load a privileged config exclusively from signed bundle roots. Project
    /// overlays are deliberately excluded so they cannot claim a system-owned
    /// provenance layer inside the config payload.
    pub fn load_bundle_config_strict_signed<T: DeserializeOwned>(
        &self,
        config_id: &str,
    ) -> std::result::Result<Option<T>, ConfigLoadError> {
        self.load_config_with_strictness_from_roots(config_id, LoadStrictness::Required, false)
            .map(|value| value.map(|(config, _)| config))
    }

    fn load_config_with_strictness_from_roots<T: DeserializeOwned>(
        &self,
        config_id: &str,
        strictness: LoadStrictness,
        include_project: bool,
    ) -> std::result::Result<Option<(T, Vec<String>)>, ConfigLoadError> {
        let subdir = Self::kind_subdir("config");
        let item_path = PathBuf::from(format!("{subdir}{config_id}.yaml"));

        // Collect (path, root_label) pairs least-specific first (bundle →
        // node → project), so later overlays yield
        // `project > node > bundle` precedence.
        let mut candidate_paths: Vec<(PathBuf, &'static str, ConfigCandidateRootClass)> =
            Vec::new();

        for bundle_root in &self.bundle_roots {
            let p = bundle_root.join(&item_path);
            if p.exists() {
                candidate_paths.push((p, "bundle", ConfigCandidateRootClass::Bundle));
            }
        }

        if include_project {
            if let Some(node_config_root) = &self.node_config_root {
                let path = node_config_root.join(&item_path);
                if path.exists() {
                    candidate_paths.push((path, "node", ConfigCandidateRootClass::Node));
                }
            }
        }

        if include_project && self.project_config_enabled {
            let p = self.project_root.join(&item_path);
            if p.exists() {
                candidate_paths.push((p, "project", ConfigCandidateRootClass::Project));
            }
        }

        if candidate_paths.is_empty() {
            return Ok(None);
        }

        let contributors: Vec<String> = candidate_paths
            .iter()
            .map(|(_, label, _)| label.to_string())
            .collect();

        if candidate_paths.len() == 1 {
            let (path, _, root_class) = &candidate_paths[0];
            let verified = self
                .load_verified_with_trust_store(
                    "config",
                    path,
                    strictness,
                    self.trust_store_for_config_root(*root_class),
                )
                .map_err(|e| ConfigLoadError::VerifyFailed {
                    path: path.clone(),
                    source: e,
                })?;
            let raw_value: serde_yaml::Value =
                serde_yaml::from_str(&verified.content).map_err(|e| {
                    ConfigLoadError::RawYamlParseFailed {
                        path: path.clone(),
                        source: e,
                    }
                })?;
            let value = serde_yaml::from_value(raw_value).map_err(|e| {
                ConfigLoadError::TypedParseFailed {
                    path: path.clone(),
                    source: e,
                }
            })?;
            return Ok(Some((value, contributors)));
        }

        // Multi-root merge path — preserve existing merge logic but return
        // all contributors so callers can apply trust policy.
        let mut merged = serde_yaml::Value::Null;
        for (path, _, root_class) in &candidate_paths {
            let verified = self
                .load_verified_with_trust_store(
                    "config",
                    path,
                    strictness,
                    self.trust_store_for_config_root(*root_class),
                )
                .map_err(|e| ConfigLoadError::VerifyFailed {
                    path: path.clone(),
                    source: e,
                })?;
            let value =
                serde_yaml::from_str::<serde_yaml::Value>(&verified.content).map_err(|e| {
                    ConfigLoadError::RawYamlParseFailed {
                        path: path.clone(),
                        source: e,
                    }
                })?;
            merged = deep_merge_yaml(merged, value);
        }

        let last_path = candidate_paths
            .last()
            .map(|(p, _, _)| p.clone())
            .unwrap_or_else(|| item_path.clone());
        let value =
            serde_yaml::from_value::<T>(merged).map_err(|e| ConfigLoadError::TypedParseFailed {
                path: last_path,
                source: e,
            })?;
        Ok(Some((value, contributors)))
    }

    pub fn scan_kind(&self, kind: &str) -> Result<Vec<ScannedItem>> {
        let subdir = Self::kind_subdir(kind);
        let mut seen_names: HashSet<String> = HashSet::new();
        let mut results = Vec::new();

        let roots_to_scan: Vec<(&Path, &str)> = {
            let mut v = Vec::new();
            v.push((self.project_root.as_path(), "project"));
            for sr in &self.bundle_roots {
                v.push((sr.as_path(), "bundle"));
            }
            v
        };

        for (root, _space) in &roots_to_scan {
            let dir = root.join(subdir);
            if !dir.is_dir() {
                continue;
            }

            let entries =
                fs::read_dir(&dir).with_context(|| format!("scanning {}", dir.display()))?;

            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }

                let name = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };

                if !seen_names.contains(&name) {
                    seen_names.insert(name.clone());
                    results.push(ScannedItem {
                        name,
                        path: path.clone(),
                        root: root.to_path_buf(),
                    });
                }
            }
        }

        Ok(results)
    }
}

fn deep_merge_yaml(base: serde_yaml::Value, overlay: serde_yaml::Value) -> serde_yaml::Value {
    use serde_yaml::Value as Yv;
    match (base, overlay) {
        (Yv::Mapping(mut base_map), Yv::Mapping(overlay_map)) => {
            for (key, value) in overlay_map {
                let merged = match base_map.remove(&key) {
                    Some(base_val) => deep_merge_yaml(base_val, value),
                    None => value,
                };
                base_map.insert(key, merged);
            }
            Yv::Mapping(base_map)
        }
        (_, overlay) => overlay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::SigningKey;
    use ryeos_engine::project_content::{AuthoritativeProjectContent, ProjectContentEntry};
    use std::collections::BTreeMap;
    use std::fs;

    #[derive(Default)]
    struct TestProjectContent {
        files: BTreeMap<PathBuf, Vec<u8>>,
    }

    impl TestProjectContent {
        fn with_file(mut self, path: &str, bytes: &[u8]) -> Self {
            self.files.insert(PathBuf::from(path), bytes.to_vec());
            self
        }
    }

    impl AuthoritativeProjectContent for TestProjectContent {
        fn list_files(
            &self,
            prefix: &Path,
            recursive: bool,
            max_entries: usize,
        ) -> Result<Vec<ProjectContentEntry>, ryeos_engine::error::EngineError> {
            let mut entries = Vec::new();
            for (path, bytes) in &self.files {
                let Ok(relative) = path.strip_prefix(prefix) else {
                    continue;
                };
                if relative.as_os_str().is_empty()
                    || (!recursive && relative.components().count() != 1)
                {
                    continue;
                }
                if entries.len() >= max_entries {
                    return Err(ryeos_engine::error::EngineError::Internal(
                        "test project content entry bound exceeded".to_string(),
                    ));
                }
                entries.push(ProjectContentEntry {
                    relative_path: relative.to_path_buf(),
                    content_hash: lillux::sha256_hex(bytes),
                    size: bytes.len() as u64,
                    normalized_mode: 0o644,
                });
            }
            entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
            Ok(entries)
        }

        fn read_file(
            &self,
            relative_path: &Path,
            max_bytes: u64,
        ) -> Result<Option<Vec<u8>>, ryeos_engine::error::EngineError> {
            Ok(self
                .files
                .get(relative_path)
                .filter(|bytes| bytes.len() as u64 <= max_bytes)
                .cloned())
        }

        fn validates_file(
            &self,
            relative_path: &Path,
            content_hash: &str,
        ) -> Result<bool, ryeos_engine::error::EngineError> {
            Ok(self
                .files
                .get(relative_path)
                .is_some_and(|bytes| lillux::sha256_hex(bytes) == content_hash))
        }

        fn validates_absence(
            &self,
            relative_path: &Path,
        ) -> Result<bool, ryeos_engine::error::EngineError> {
            Ok(!self.files.contains_key(relative_path))
        }
    }

    fn create_file(dir: &Path, relative: &str, content: &str) -> PathBuf {
        let p = dir.join(relative);
        if let Some(d) = p.parent() {
            fs::create_dir_all(d).unwrap()
        }
        fs::write(&p, content).unwrap();
        p
    }

    /// Node trusted-keys dir for tests that don't exercise operator
    /// trust. Nonexistent path — `TrustStore::load` skips non-dirs.
    fn no_operator_trust() -> PathBuf {
        PathBuf::from("/nonexistent-operator-trust")
    }

    fn trust_document(signing_key: &SigningKey) -> String {
        let fingerprint = lillux::signature::compute_fingerprint(&signing_key.verifying_key());
        let vk_bytes = signing_key.verifying_key().to_bytes();
        let pem_b64 = base64::engine::general_purpose::STANDARD.encode(
            [
                0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
            ]
            .as_slice()
            .iter()
            .chain(vk_bytes.iter())
            .copied()
            .collect::<Vec<u8>>(),
        );
        format!(
            r#"version = "1.0.0"
category = "keys/trusted"
fingerprint = "{fingerprint}"
owner = "test"

[public_key]
pem = """
-----BEGIN PUBLIC KEY-----
{pem_b64}
-----END PUBLIC KEY-----
"""
"#,
        )
    }

    fn create_trust_store(dir: &Path, signing_key: &SigningKey) {
        let fingerprint = lillux::signature::compute_fingerprint(&signing_key.verifying_key());
        let toml_content = trust_document(signing_key);
        create_file(
            dir,
            &format!(".ai/config/keys/trusted/{fingerprint}.toml"),
            &toml_content,
        );
    }

    fn sign_md(body: &str, signing_key: &SigningKey) -> String {
        lillux::signature::sign_content(body, signing_key, "<!--", Some("-->"))
    }

    #[test]
    fn load_config_project_overrides_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let bundle = tmp.path().join("bundle");

        create_file(&bundle, ".ai/config/test.yaml", "name: bundle\n");
        create_file(&project, ".ai/config/test.yaml", "name: project\n");

        let loader = VerifiedLoader::new(project, vec![bundle], &no_operator_trust()).unwrap();
        let config: serde_yaml::Value = loader.load_config_strict("test").unwrap().unwrap();

        assert_eq!(config["name"], "project");
    }

    #[test]
    fn load_config_missing_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");

        let loader = VerifiedLoader::new(project, vec![], &no_operator_trust()).unwrap();
        let config = loader
            .load_config_strict::<serde_yaml::Value>("nonexistent")
            .unwrap();

        assert!(config.is_none());
    }

    #[test]
    fn load_config_bad_yaml_returns_err() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");

        create_file(&project, ".ai/config/bad.yaml", "not valid yaml: [");

        let loader = VerifiedLoader::new(project, vec![], &no_operator_trust()).unwrap();
        let result = loader.load_config_strict::<serde_yaml::Value>("bad");

        assert!(
            result.is_err(),
            "bad YAML should fail, not silently return None"
        );
    }

    #[test]
    fn load_config_bundle_only() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let bundle = tmp.path().join("bundle");

        create_file(&bundle, ".ai/config/defaults.yaml", "key: from_bundle\n");

        let loader = VerifiedLoader::new(project, vec![bundle], &no_operator_trust()).unwrap();
        let config: serde_yaml::Value = loader.load_config_strict("defaults").unwrap().unwrap();

        assert_eq!(config["key"], "from_bundle");
    }

    #[test]
    fn signed_config_proof_detects_new_project_shadow() {
        let signing_key = SigningKey::from_bytes(&[42u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let bundle = tmp.path().join("bundle");
        create_trust_store(&project, &signing_key);
        let bundle_body = "name: bundle\n";
        create_file(
            &bundle,
            ".ai/config/test.yaml",
            &lillux::signature::sign_content(bundle_body, &signing_key, "#", None),
        );
        let loader = VerifiedLoader::new(
            project.clone(),
            vec![bundle],
            &project.join(".ai/config/keys/trusted"),
        )
        .unwrap();
        let first = loader
            .load_config_strict_signed_with_proof::<serde_yaml::Value>("test")
            .unwrap()
            .unwrap();
        assert_eq!(first.value["name"], "bundle");
        assert!(first.dependency_proof.revalidate_current());
        create_file(
            &project,
            ".ai/config/test.yaml",
            &lillux::signature::sign_content("name: project\n", &signing_key, "#", None),
        );
        assert!(!first.dependency_proof.revalidate_current());
        let second = loader
            .load_config_strict_signed_with_proof::<serde_yaml::Value>("test")
            .unwrap()
            .unwrap();
        assert_eq!(second.value["name"], "project");
        assert_ne!(
            first.dependency_proof.identity_digest().unwrap(),
            second.dependency_proof.identity_digest().unwrap()
        );
    }

    #[test]
    fn signed_config_proof_revalidates_project_trust_from_admitted_content() {
        let config_bytes = b"# signed fixture\nname: project\n";
        let trust_bytes = b"version = \"1.0.0\"\n";
        let proof = ConfigDependencyProof {
            config_id: "test".to_string(),
            effective_trust_identity: "effective".to_string(),
            node_trust_identity: "node".to_string(),
            project_trust_sources: vec![TrustSourceDependency {
                logical_path: PathBuf::from("publisher.toml"),
                source_hash: lillux::sha256_hex(trust_bytes),
            }],
            candidates: vec![ConfigCandidateDependency {
                root_class: ConfigCandidateRootClass::Project,
                root_index: 0,
                logical_path: PathBuf::from(".ai/config/test.yaml"),
                path: PathBuf::from("/must/not/be/opened/test.yaml"),
                state: ConfigCandidateState::Present {
                    source_hash: lillux::sha256_hex(config_bytes),
                    content_hash: "content".to_string(),
                    signer_fingerprint: Some("publisher".to_string()),
                },
            }],
        };
        let project_root = PathBuf::from("/not-opened/project");
        let exact = TestProjectContent::default()
            .with_file(".ai/config/test.yaml", config_bytes)
            .with_file(".ai/config/keys/trusted/publisher.toml", trust_bytes);
        assert!(proof.revalidate_under_project_authority(Some(&project_root), None, Some(&exact),));

        let revoked = TestProjectContent::default().with_file(".ai/config/test.yaml", config_bytes);
        assert!(!proof.revalidate_under_project_authority(
            Some(&project_root),
            None,
            Some(&revoked),
        ));
        assert_eq!(
            proof.revalidate_under_project_authority_status(
                Some(&project_root),
                None,
                Some(&revoked),
            ),
            ConfigDependencyProofStatus::ImmutableAuthorityMismatch
        );

        let replaced = TestProjectContent::default()
            .with_file(".ai/config/test.yaml", config_bytes)
            .with_file(
                ".ai/config/keys/trusted/publisher.toml",
                b"version = \"changed\"\n",
            );
        assert!(!proof.revalidate_under_project_authority(
            Some(&project_root),
            None,
            Some(&replaced),
        ));
    }

    #[test]
    fn signed_config_proof_classifies_node_change_as_mutable() {
        let tmp = tempfile::tempdir().unwrap();
        let node_root = tmp.path().join("node");
        create_file(&node_root, "test.yaml", "name: original\n");
        let original = b"name: original\n";
        let proof = ConfigDependencyProof {
            config_id: "test".to_string(),
            effective_trust_identity: "effective".to_string(),
            node_trust_identity: "node".to_string(),
            project_trust_sources: Vec::new(),
            candidates: vec![ConfigCandidateDependency {
                root_class: ConfigCandidateRootClass::Node,
                root_index: 0,
                logical_path: PathBuf::from("test.yaml"),
                path: node_root.join("test.yaml"),
                state: ConfigCandidateState::Present {
                    source_hash: lillux::sha256_hex(original),
                    content_hash: "content".to_string(),
                    signer_fingerprint: Some("publisher".to_string()),
                },
            }],
        };
        assert_eq!(
            proof.revalidate_under_project_authority_status(None, Some(&node_root), None),
            ConfigDependencyProofStatus::Current
        );
        create_file(&node_root, "test.yaml", "name: changed\n");
        assert_eq!(
            proof.revalidate_under_project_authority_status(None, Some(&node_root), None),
            ConfigDependencyProofStatus::MutableAuthorityChanged
        );
    }

    #[test]
    fn project_trust_cannot_admit_bundle_or_node_config() {
        let project_key = SigningKey::from_bytes(&[51u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let node_config = tmp.path().join("node-config");
        let node_trust = tmp.path().join("node-trust");
        let bundle = tmp.path().join("bundle");
        create_trust_store(&project, &project_key);
        create_file(
            &bundle,
            ".ai/config/test.yaml",
            &lillux::signature::sign_content("name: bundle\n", &project_key, "#", None),
        );
        let loader = VerifiedLoader::new_with_node_config(
            project.clone(),
            Some(node_config.clone()),
            vec![bundle],
            &node_trust,
        )
        .unwrap();
        let error = loader
            .load_config_strict_signed_with_proof::<serde_yaml::Value>("test")
            .unwrap_err();
        assert!(error.to_string().contains("unknown signer"));

        create_file(
            &node_config,
            ".ai/config/node-only.yaml",
            &lillux::signature::sign_content("name: node\n", &project_key, "#", None),
        );
        let error = loader
            .load_config_strict_signed_with_proof::<serde_yaml::Value>("node-only")
            .unwrap_err();
        assert!(error.to_string().contains("unknown signer"));

        create_file(
            &project,
            ".ai/config/project-only.yaml",
            &lillux::signature::sign_content("name: project\n", &project_key, "#", None),
        );
        let project_config = loader
            .load_config_strict_signed_with_proof::<serde_yaml::Value>("project-only")
            .unwrap()
            .unwrap();
        assert_eq!(project_config.value["name"], "project");
    }

    #[test]
    fn trust_document_fingerprint_must_match_decoded_key() {
        let signing_key = SigningKey::from_bytes(&[52u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        create_trust_store(tmp.path(), &signing_key);
        let trust_dir = tmp.path().join(".ai/config/keys/trusted");
        let entry = fs::read_dir(&trust_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let declared = lillux::crypto::fingerprint(&signing_key.verifying_key());
        let content = fs::read_to_string(&entry).unwrap();
        fs::write(&entry, content.replace(&declared, &"f".repeat(64))).unwrap();

        let error = TrustStore::load(tmp.path(), &no_operator_trust()).unwrap_err();
        assert!(format!("{error:#}").contains("declared fingerprint"));
    }

    #[test]
    fn load_verified_strips_signature_and_hashes() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        create_trust_store(tmp.path(), &sk);

        let body = "# Hello\n\nBody text.\n";
        let signed = sign_md(body, &sk);
        let path = tmp.path().join("test.md");
        fs::write(&path, &signed).unwrap();

        let loader =
            VerifiedLoader::new(tmp.path().to_path_buf(), vec![], &no_operator_trust()).unwrap();
        let verified = loader.load_verified("directive", &path).unwrap();

        assert!(!verified.content.contains("ryeos:signed:"));
        assert!(verified.content.contains("# Hello"));
        assert_eq!(verified.hash.len(), 64);
    }

    #[test]
    fn load_verified_unsigned_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("plain.md");
        let content = "# Plain Directive\n\nSome content here.\n";
        fs::write(&path, content).unwrap();

        let loader =
            VerifiedLoader::new(tmp.path().to_path_buf(), vec![], &no_operator_trust()).unwrap();
        let verified = loader.load_verified("directive", &path).unwrap();

        assert_eq!(verified.content, content);
        assert_eq!(verified.hash.len(), 64);
    }

    #[test]
    fn load_verified_rejects_tampered_content() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        create_trust_store(tmp.path(), &sk);

        let body = "# Original\n";
        let signed = sign_md(body, &sk);
        let tampered = signed.replace("# Original", "# Tampered");
        let path = tmp.path().join("tampered.md");
        fs::write(&path, &tampered).unwrap();

        let loader =
            VerifiedLoader::new(tmp.path().to_path_buf(), vec![], &no_operator_trust()).unwrap();
        let result = loader.load_verified("directive", &path);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("content hash mismatch"));
    }

    #[test]
    fn load_verified_rejects_bad_signature() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let other_sk = SigningKey::from_bytes(&[99u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        create_trust_store(tmp.path(), &other_sk);

        let body = "# Test\n";
        let signed = sign_md(body, &sk);
        let sk_fp = lillux::signature::compute_fingerprint(&sk.verifying_key());
        let other_fp = lillux::signature::compute_fingerprint(&other_sk.verifying_key());
        let forged = signed.replace(&sk_fp, &other_fp);
        let path = tmp.path().join("bad_sig.md");
        fs::write(&path, &forged).unwrap();

        let loader =
            VerifiedLoader::new(tmp.path().to_path_buf(), vec![], &no_operator_trust()).unwrap();
        let result = loader.load_verified("directive", &path);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("signature verification failed"));
    }

    #[test]
    fn load_verified_accepts_unknown_signer() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        let body = "# Test\n";
        let signed = sign_md(body, &sk);
        let path = tmp.path().join("unknown_signer.md");
        fs::write(&path, &signed).unwrap();

        let loader =
            VerifiedLoader::new(tmp.path().to_path_buf(), vec![], &no_operator_trust()).unwrap();
        let verified = loader.load_verified("directive", &path).unwrap();

        assert!(verified.content.contains("# Test"));
    }

    #[test]
    fn trust_store_loads_operator_and_project_dirs() {
        let op_sk = SigningKey::from_bytes(&[42u8; 32]);
        let proj_sk = SigningKey::from_bytes(&[43u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        let operator_root = tmp.path().join("app-root");
        let project = tmp.path().join("project");
        create_trust_store(&operator_root, &op_sk);
        create_trust_store(&project, &proj_sk);

        let store =
            TrustStore::load(&project, &operator_root.join(".ai/config/keys/trusted")).unwrap();

        assert_eq!(store.len(), 2);
        let op_fp = lillux::signature::compute_fingerprint(&op_sk.verifying_key());
        let proj_fp = lillux::signature::compute_fingerprint(&proj_sk.verifying_key());
        assert!(store.get(&op_fp).is_some());
        assert!(store.get(&proj_fp).is_some());
    }

    fn padded_trust_document(signing_key: &SigningKey, target_bytes: usize) -> Vec<u8> {
        let mut document = trust_document(signing_key).into_bytes();
        assert!(document.len().saturating_add(2) <= target_bytes);
        document.push(b'#');
        document.resize(target_bytes.saturating_sub(1), b'x');
        document.push(b'\n');
        assert_eq!(document.len(), target_bytes);
        document
    }

    #[test]
    fn trust_store_enforces_exact_document_count_boundary() {
        let signing_key = SigningKey::from_bytes(&[61u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        let trust_dir = tmp.path().join("trusted");
        fs::create_dir_all(&trust_dir).unwrap();
        let document = trust_document(&signing_key);
        for index in 0..MAX_TRUST_DOCUMENTS {
            fs::write(trust_dir.join(format!("{index:04}.toml")), &document).unwrap();
        }
        let exact = TrustStore::load(Path::new("/missing-project"), &trust_dir).unwrap();
        assert_eq!(exact.len(), 1);

        fs::write(
            trust_dir.join(format!("{:04}.toml", MAX_TRUST_DOCUMENTS)),
            &document,
        )
        .unwrap();
        let error = TrustStore::load(Path::new("/missing-project"), &trust_dir).unwrap_err();
        assert!(
            format!("{error:#}").contains(&format!("exceeds {MAX_TRUST_DOCUMENTS} regular files"))
        );
    }

    #[test]
    fn trust_store_enforces_exact_document_size_boundary() {
        let signing_key = SigningKey::from_bytes(&[62u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        let trust_dir = tmp.path().join("trusted");
        fs::create_dir_all(&trust_dir).unwrap();
        let exact = padded_trust_document(
            &signing_key,
            usize::try_from(MAX_TRUST_DOCUMENT_BYTES).unwrap(),
        );
        let path = trust_dir.join("exact.toml");
        fs::write(&path, &exact).unwrap();
        assert_eq!(
            TrustStore::load(Path::new("/missing-project"), &trust_dir)
                .unwrap()
                .len(),
            1
        );

        let mut oversized = exact;
        oversized.push(b'\n');
        fs::write(path, oversized).unwrap();
        let error = TrustStore::load(Path::new("/missing-project"), &trust_dir).unwrap_err();
        assert!(format!("{error:#}").contains(&format!("exceeds {MAX_TRUST_DOCUMENT_BYTES} bytes")));
    }

    #[test]
    fn trust_store_enforces_exact_aggregate_size_boundary() {
        let signing_key = SigningKey::from_bytes(&[63u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        let trust_dir = tmp.path().join("trusted");
        fs::create_dir_all(&trust_dir).unwrap();
        let document = padded_trust_document(
            &signing_key,
            usize::try_from(MAX_TRUST_DOCUMENT_BYTES).unwrap(),
        );
        let exact_count =
            usize::try_from(MAX_TRUST_DIRECTORY_BYTES / MAX_TRUST_DOCUMENT_BYTES).unwrap();
        for index in 0..exact_count {
            fs::write(trust_dir.join(format!("{index:02}.toml")), &document).unwrap();
        }
        assert_eq!(
            TrustStore::load(Path::new("/missing-project"), &trust_dir)
                .unwrap()
                .len(),
            1
        );

        fs::write(trust_dir.join("overflow.toml"), &document).unwrap();
        let error = TrustStore::load(Path::new("/missing-project"), &trust_dir).unwrap_err();
        assert!(format!("{error:#}").contains(&format!(
            "exceeds {MAX_TRUST_DIRECTORY_BYTES} aggregate bytes"
        )));
    }

    #[test]
    fn trust_store_ignores_bundle_roots() {
        // A bundle shipping its own trusted-keys dir must NOT become a
        // trust authority — only project + operator dirs are consulted.
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        create_trust_store(&bundle, &sk);

        let loader = VerifiedLoader::new(
            tmp.path().join("project"),
            vec![bundle],
            &no_operator_trust(),
        )
        .unwrap();

        assert!(
            loader.trust_store().is_empty(),
            "bundle-shipped trust dirs must be ignored"
        );
    }

    #[test]
    fn trust_store_empty_when_no_dirs() {
        let store =
            TrustStore::load(Path::new("/nonexistent"), Path::new("/also-nonexistent")).unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn load_verified_hash_is_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("det.md");
        let content = "deterministic content";
        fs::write(&path, content).unwrap();

        let loader =
            VerifiedLoader::new(tmp.path().to_path_buf(), vec![], &no_operator_trust()).unwrap();
        let v1 = loader.load_verified("directive", &path).unwrap();
        let v2 = loader.load_verified("directive", &path).unwrap();

        assert_eq!(v1.hash, v2.hash);
    }

    #[test]
    fn scan_kind_finds_across_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let bundle = tmp.path().join("bundle");

        create_file(&bundle, ".ai/tools/bundle_tool.md", "# Bundle Tool\n");
        create_file(&bundle, ".ai/tools/shared.md", "# Bundle Shared\n");
        create_file(&project, ".ai/tools/proj_tool.md", "# Project Tool\n");
        create_file(&project, ".ai/tools/shared.md", "# Project Shared\n");

        let project_clone = project.clone();
        let loader = VerifiedLoader::new(project, vec![bundle], &no_operator_trust()).unwrap();
        let items = loader.scan_kind("tool").unwrap();
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();

        assert!(names.contains(&"bundle_tool"));
        assert!(names.contains(&"proj_tool"));
        assert!(names.contains(&"shared"));

        // Project is scanned first, so a name present in both roots is
        // attributed to the project root (first-found-wins for enumeration).
        let shared = items.iter().find(|i| i.name == "shared").unwrap();
        assert_eq!(shared.root, project_clone);
    }

    #[test]
    fn scan_kind_empty_when_no_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");

        let loader = VerifiedLoader::new(project, vec![], &no_operator_trust()).unwrap();
        let items = loader.scan_kind("directive").unwrap();

        assert!(items.is_empty());
    }

    // ── Strict mode tests ──────────────────────────────────────────────

    /// Helper: sign YAML with a test key and pin it into `trust_dir`
    /// (a trusted-keys dir, written as-is) so strict mode accepts it.
    fn sign_and_pin(yaml_body: &str, trust_dir: &Path) -> String {
        use base64::Engine;
        use ed25519_dalek::SigningKey;
        use lillux::signature::{compute_fingerprint, sign_content_at};

        let sk = SigningKey::from_bytes(&[99u8; 32]);
        let vk = sk.verifying_key();
        let fp = compute_fingerprint(&vk);
        let signed = sign_content_at(yaml_body, &sk, "#", None, "2026-01-01T00:00:00Z");

        std::fs::create_dir_all(trust_dir).unwrap();
        let vk_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
        let toml =
            format!("fingerprint = \"{fp}\"\npem = \"ed25519:{vk_b64}\"\nowner = \"test\"\n");
        std::fs::write(trust_dir.join("test.toml"), toml).unwrap();
        signed
    }

    #[test]
    fn strict_load_rejects_unsigned_config() {
        let tmp = tempfile::tempdir().unwrap();
        let system = tmp.path().join("system");
        let cfg_subpath = ".ai/config/ryeos-runtime/model-providers/test.yaml";
        std::fs::create_dir_all(system.join(cfg_subpath).parent().unwrap()).unwrap();
        // NO signature header.
        std::fs::write(
            system.join(cfg_subpath),
            "base_url: https://example.com/v1\n",
        )
        .unwrap();

        let loader = VerifiedLoader::new(
            tmp.path().join("project"),
            vec![system],
            &no_operator_trust(),
        )
        .unwrap();
        let res = loader
            .load_config_strict_signed::<serde_yaml::Value>("ryeos-runtime/model-providers/test");
        assert!(res.is_err(), "strict mode must reject unsigned config");
        let msg = format!("{:#}", res.unwrap_err());
        assert!(
            msg.contains("unsigned") || msg.contains("REJECTED"),
            "error must explain unsigned rejection: {msg}"
        );
    }

    #[test]
    fn strict_load_rejects_unknown_signer() {
        let tmp = tempfile::tempdir().unwrap();
        let system = tmp.path().join("system");
        let cfg_subpath = ".ai/config/ryeos-runtime/model-providers/test.yaml";
        std::fs::create_dir_all(system.join(cfg_subpath).parent().unwrap()).unwrap();

        // Sign with a throwaway key that is NOT in the trust store.
        let yaml_body = "base_url: https://example.com/v1\n";
        let sk = ed25519_dalek::SigningKey::from_bytes(&[77u8; 32]);
        let signed =
            lillux::signature::sign_content_at(yaml_body, &sk, "#", None, "2026-01-01T00:00:00Z");
        std::fs::write(system.join(cfg_subpath), signed).unwrap();

        let loader = VerifiedLoader::new(
            tmp.path().join("project"),
            vec![system],
            &no_operator_trust(),
        )
        .unwrap();
        let res = loader
            .load_config_strict_signed::<serde_yaml::Value>("ryeos-runtime/model-providers/test");
        assert!(res.is_err(), "strict mode must reject unknown signer");
        let msg = format!("{:#}", res.unwrap_err());
        assert!(
            msg.contains("unknown signer") || msg.contains("REJECTED"),
            "error must explain unknown-signer rejection: {msg}"
        );
    }

    #[test]
    fn strict_load_accepts_operator_trusted_signed_config() {
        let tmp = tempfile::tempdir().unwrap();
        let system = tmp.path().join("system");
        let operator_keys = tmp.path().join("app-root/.ai/config/keys/trusted");
        let cfg_subpath = ".ai/config/ryeos-runtime/model-providers/test.yaml";
        std::fs::create_dir_all(system.join(cfg_subpath).parent().unwrap()).unwrap();

        let yaml_body = "base_url: https://example.com/v1\n";
        let signed = sign_and_pin(yaml_body, &operator_keys);
        std::fs::write(system.join(cfg_subpath), signed).unwrap();

        let loader =
            VerifiedLoader::new(tmp.path().join("project"), vec![system], &operator_keys).unwrap();
        let res = loader
            .load_config_strict_signed::<serde_yaml::Value>("ryeos-runtime/model-providers/test");
        assert!(
            res.is_ok(),
            "strict mode must accept config signed by an operator-trusted key"
        );
        let val = res.unwrap().expect("should have a value");
        assert_eq!(val["base_url"].as_str(), Some("https://example.com/v1"));
    }

    #[test]
    fn strict_load_rejects_bundle_pinned_signer() {
        // The signer is pinned ONLY inside the bundle's own
        // `.ai/config/keys/trusted` — the removed self-vouching path.
        // Strict verification must treat it as an unknown signer.
        let tmp = tempfile::tempdir().unwrap();
        let system = tmp.path().join("system");
        let cfg_subpath = ".ai/config/ryeos-runtime/model-providers/test.yaml";
        std::fs::create_dir_all(system.join(cfg_subpath).parent().unwrap()).unwrap();

        let yaml_body = "base_url: https://example.com/v1\n";
        let signed = sign_and_pin(yaml_body, &system.join(".ai/config/keys/trusted"));
        std::fs::write(system.join(cfg_subpath), signed).unwrap();

        let loader = VerifiedLoader::new(
            tmp.path().join("project"),
            vec![system],
            &no_operator_trust(),
        )
        .unwrap();
        let res = loader
            .load_config_strict_signed::<serde_yaml::Value>("ryeos-runtime/model-providers/test");
        assert!(
            res.is_err(),
            "bundle-pinned signer must be rejected as unknown"
        );
        let msg = format!("{:#}", res.unwrap_err());
        assert!(
            msg.contains("unknown signer") || msg.contains("REJECTED"),
            "error must explain unknown-signer rejection: {msg}"
        );
    }
}
