use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ryeos_engine::contracts::{SignatureHeader, TrustClass};
use ryeos_engine::trust::{
    SignerContext, TrustStore, TrustedSignerEntry, trusted_signers_digest,
    verify_item_signature_with_hash,
};
use serde::{Serialize, de::DeserializeOwned};
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

pub struct VerifiedLoader {
    project_root: PathBuf,
    /// The logical project of `project_root` for signer project scopes: the
    /// project as the node knows it. It differs from `project_root` when the
    /// loader reads a pinned or COW checkout. Defaults to `project_root`,
    /// which is correct for a live project.
    logical_project_root: Option<PathBuf>,
    project_config_enabled: bool,
    node_config_root: Option<PathBuf>,
    bundle_roots: Vec<PathBuf>,
    /// Trust built from the signer list the daemon handed over; the only
    /// trust source the loader has.
    trust_store: TrustStore,
    trusted_signers_digest: String,
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
    trusted_signers_digest: String,
    candidates: Vec<ConfigCandidateDependency>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigDependencyProofStatus {
    Current,
    MutableAuthorityChanged,
    ImmutableAuthorityMismatch,
}

impl ConfigDependencyProof {
    /// Whether the signer list this proof was captured under hashes to
    /// `trusted_signers_digest`.
    pub fn trusted_signers_match(&self, trusted_signers_digest: &str) -> bool {
        self.trusted_signers_digest == trusted_signers_digest
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
            trusted_signers_digest: &'a str,
            candidates: Vec<StableCandidate<'a>>,
        }
        let value = serde_json::to_value(StableProof {
            schema_version: 2,
            config_id: &self.config_id,
            trusted_signers_digest: &self.trusted_signers_digest,
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
    /// closure. The verified loader owns config discovery policy; this
    /// method only asks the generic authority to prove the resulting positive
    /// and negative dependencies. Node config remains mutable node-local
    /// authority; the trusted signer list is compared by the caller through
    /// [`ConfigDependencyProof::trusted_signers_match`].
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
            .saturating_add(self.trusted_signers_digest.capacity())
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
    fn root_class_for_path(&self, path: &Path) -> ConfigCandidateRootClass {
        if self.bundle_roots.iter().any(|root| path.starts_with(root)) {
            ConfigCandidateRootClass::Bundle
        } else if self
            .node_config_root
            .as_deref()
            .is_some_and(|root| path.starts_with(root))
        {
            ConfigCandidateRootClass::Node
        } else if self.project_config_enabled && path.starts_with(&self.project_root) {
            ConfigCandidateRootClass::Project
        } else {
            ConfigCandidateRootClass::Node
        }
    }

    fn signer_context<'a>(
        &'a self,
        kind: &'a str,
        root_class: ConfigCandidateRootClass,
    ) -> SignerContext<'a> {
        match root_class {
            ConfigCandidateRootClass::Project => {
                SignerContext::project(kind, self.logical_project_root.as_deref())
            }
            ConfigCandidateRootClass::Node => SignerContext::node(kind),
            ConfigCandidateRootClass::Bundle => SignerContext::bundle(kind),
        }
    }

    fn build_trust(trusted_signers: &[TrustedSignerEntry]) -> Result<(TrustStore, String)> {
        let trust_store = TrustStore::from_signer_entries(trusted_signers)
            .context("build runtime trust from launch envelope signers")?;
        Ok((trust_store, trusted_signers_digest(trusted_signers)))
    }

    /// `bundle_roots` are CONFIG search roots only (configs ship in
    /// bundles); trust comes exclusively from `trusted_signers`, the scoped
    /// signer list the daemon carries in the launch envelope. No directory
    /// is read for trust and no hidden env reads happen here.
    pub fn new(
        project_root: PathBuf,
        bundle_roots: Vec<PathBuf>,
        trusted_signers: &[TrustedSignerEntry],
    ) -> Result<Self> {
        Self::new_with_node_config(project_root, None, bundle_roots, trusted_signers)
    }

    /// Construct with an explicit node-local config layer. Config merges in
    /// bundle -> node -> project order; signer trust is the one signer list,
    /// judged per root by each signer's scope.
    pub fn new_with_node_config(
        project_root: PathBuf,
        node_config_root: Option<PathBuf>,
        bundle_roots: Vec<PathBuf>,
        trusted_signers: &[TrustedSignerEntry],
    ) -> Result<Self> {
        let (trust_store, trusted_signers_digest) = Self::build_trust(trusted_signers)?;
        Ok(Self {
            logical_project_root: Some(project_root.clone()),
            project_root,
            project_config_enabled: true,
            node_config_root,
            bundle_roots,
            trust_store,
            trusted_signers_digest,
        })
    }

    /// Construct for a projectless execution. Its scratch cwd is deliberately
    /// excluded from config resolution.
    pub fn new_projectless_with_node_config(
        node_config_root: Option<PathBuf>,
        bundle_roots: Vec<PathBuf>,
        trusted_signers: &[TrustedSignerEntry],
    ) -> Result<Self> {
        let (trust_store, trusted_signers_digest) = Self::build_trust(trusted_signers)?;
        Ok(Self {
            project_root: node_config_root.clone().unwrap_or_default(),
            logical_project_root: None,
            project_config_enabled: false,
            node_config_root,
            bundle_roots,
            trust_store,
            trusted_signers_digest,
        })
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// State the logical project this loader's project root belongs to, when
    /// the root is a pinned or COW checkout rather than the project itself.
    /// `None` states that the logical project is unknown, so signers scoped to
    /// specific projects admit nothing.
    pub fn with_logical_project_of(
        self,
        roots: &ryeos_engine::item_resolution::ResolutionRoots,
    ) -> Self {
        match roots.logical_project_root.clone() {
            Some(logical) => self.with_logical_project_root(Some(logical)),
            None => self,
        }
    }

    /// As [`Self::with_logical_project_of`] with an explicit value.
    pub fn with_logical_project_root(mut self, logical_project_root: Option<PathBuf>) -> Self {
        self.logical_project_root = logical_project_root;
        self
    }

    pub fn trust_store(&self) -> &TrustStore {
        &self.trust_store
    }

    /// Digest of the signer list this loader verifies against. Dependency
    /// proofs captured by this loader carry it.
    pub fn trusted_signers_digest(&self) -> &str {
        &self.trusted_signers_digest
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
        self.load_verified_from_root(kind, path, strictness, self.root_class_for_path(path))
    }

    fn load_verified_from_root(
        &self,
        kind: &str,
        path: &Path,
        strictness: LoadStrictness,
        root_class: ConfigCandidateRootClass,
    ) -> Result<VerifiedContent> {
        let raw = String::from_utf8(
            lillux::read_regular_file_bounded_no_follow(path, MAX_CONFIG_SOURCE_BYTES)
                .with_context(|| format!("securely reading {}", path.display()))?,
        )
        .with_context(|| format!("securely reading {}", path.display()))?;
        self.load_verified_content_from_root(kind, path, raw, strictness, root_class)
    }

    fn load_verified_content_from_root(
        &self,
        kind: &str,
        path: &Path,
        raw: String,
        strictness: LoadStrictness,
        root_class: ConfigCandidateRootClass,
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

            let header = SignatureHeader {
                timestamp: sig_header.timestamp.clone(),
                content_hash: sig_header.content_hash.clone(),
                signature_b64: sig_header.signature_b64.clone(),
                signer_fingerprint: sig_header.signer_fingerprint.clone(),
            };
            let context = self.signer_context(kind, root_class);
            let (trust_class, _) =
                verify_item_signature_with_hash(&hash, &header, &self.trust_store, &context)
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "signature verification failed for {} (signer: {}): {e}",
                            path.display(),
                            sig_header.signer_fingerprint
                        )
                    })?;
            if trust_class == TrustClass::Trusted {
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
                            "signed by unknown signer {} - not trusted for this root: {}",
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
                             (not in the trusted signer list, or outside its scope). \
                             Strict mode requires a trusted publisher signature for \
                             this config kind.",
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
                         --key .dev-keys/PUBLISHER_DEV.pem --owner \"RyeOS Development\"",
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
            if prefix != "#"
                && let Some(header) = lillux::signature::parse_signature_line(line, "#", None)
            {
                return Some(header);
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
                .load_verified_from_root("config", path, LoadStrictness::Permissive, *root_class)
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
                .load_verified_from_root("config", path, LoadStrictness::Permissive, *root_class)
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
            trusted_signers_digest: self.trusted_signers_digest.clone(),
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
                    .load_verified_content_from_root(
                        "config",
                        &path,
                        raw,
                        LoadStrictness::Required,
                        ConfigCandidateRootClass::Project,
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
                    .load_verified_from_root("config", &path, LoadStrictness::Required, root_class)
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

        if include_project && let Some(node_config_root) = &self.node_config_root {
            let path = node_config_root.join(&item_path);
            if path.exists() {
                candidate_paths.push((path, "node", ConfigCandidateRootClass::Node));
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
                .load_verified_from_root("config", path, strictness, *root_class)
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
                .load_verified_from_root("config", path, strictness, *root_class)
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
            if root.as_os_str().is_empty() {
                continue;
            }
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
    use base64::Engine;
    use lillux::crypto::SigningKey;
    use ryeos_engine::project_content::{AuthoritativeProjectContent, ProjectContentEntry};
    use ryeos_engine::trust::{SignerScope, SignerSpace};
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

    /// Signer list for tests that trust nobody.
    fn no_signers() -> Vec<TrustedSignerEntry> {
        Vec::new()
    }

    fn signer_entry(signing_key: &SigningKey, scope: SignerScope) -> TrustedSignerEntry {
        let verifying_key = signing_key.verifying_key();
        TrustedSignerEntry {
            fingerprint: lillux::signature::compute_fingerprint(&verifying_key),
            public_key: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(verifying_key.to_bytes())
            ),
            label: "test".to_string(),
            scope,
        }
    }

    /// One signer trusted in every space for every kind.
    fn trusted_everywhere(signing_key: &SigningKey) -> Vec<TrustedSignerEntry> {
        vec![signer_entry(signing_key, SignerScope::everywhere())]
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

        let loader = VerifiedLoader::new(project, vec![bundle], &no_signers()).unwrap();
        let config: serde_yaml::Value = loader.load_config_strict("test").unwrap().unwrap();

        assert_eq!(config["name"], "project");
    }

    #[test]
    fn load_config_missing_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");

        let loader = VerifiedLoader::new(project, vec![], &no_signers()).unwrap();
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

        let loader = VerifiedLoader::new(project, vec![], &no_signers()).unwrap();
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

        let loader = VerifiedLoader::new(project, vec![bundle], &no_signers()).unwrap();
        let config: serde_yaml::Value = loader.load_config_strict("defaults").unwrap().unwrap();

        assert_eq!(config["key"], "from_bundle");
    }

    #[test]
    fn signed_config_proof_detects_new_project_shadow() {
        let signing_key = SigningKey::from_bytes(&[42u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let bundle = tmp.path().join("bundle");
        let bundle_body = "name: bundle\n";
        create_file(
            &bundle,
            ".ai/config/test.yaml",
            &lillux::signature::sign_content(bundle_body, &signing_key, "#", None),
        );
        let loader = VerifiedLoader::new(
            project.clone(),
            vec![bundle],
            &trusted_everywhere(&signing_key),
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
    fn signed_config_proof_revalidates_project_config_from_admitted_content() {
        let config_bytes = b"# signed fixture\nname: project\n";
        let proof = ConfigDependencyProof {
            config_id: "test".to_string(),
            trusted_signers_digest: "signers".to_string(),
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
        let exact = TestProjectContent::default().with_file(".ai/config/test.yaml", config_bytes);
        assert!(proof.revalidate_under_project_authority(Some(&project_root), None, Some(&exact)));

        let removed = TestProjectContent::default();
        assert!(!proof.revalidate_under_project_authority(
            Some(&project_root),
            None,
            Some(&removed),
        ));
        assert_eq!(
            proof.revalidate_under_project_authority_status(
                Some(&project_root),
                None,
                Some(&removed),
            ),
            ConfigDependencyProofStatus::ImmutableAuthorityMismatch
        );

        let replaced =
            TestProjectContent::default().with_file(".ai/config/test.yaml", b"# changed\n");
        assert!(!proof.revalidate_under_project_authority(
            Some(&project_root),
            None,
            Some(&replaced),
        ));
    }

    #[test]
    fn signed_config_proof_matches_only_its_signer_digest() {
        let proof = ConfigDependencyProof {
            config_id: "test".to_string(),
            trusted_signers_digest: "signers".to_string(),
            candidates: Vec::new(),
        };
        assert!(proof.trusted_signers_match("signers"));
        assert!(!proof.trusted_signers_match("other"));
    }

    #[test]
    fn signed_config_proof_classifies_node_change_as_mutable() {
        let tmp = tempfile::tempdir().unwrap();
        let node_root = tmp.path().join("node");
        create_file(&node_root, "test.yaml", "name: original\n");
        let original = b"name: original\n";
        let proof = ConfigDependencyProof {
            config_id: "test".to_string(),
            trusted_signers_digest: "signers".to_string(),
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
    fn project_scoped_signer_cannot_admit_bundle_or_node_config() {
        let project_key = SigningKey::from_bytes(&[51u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let node_config = tmp.path().join("node-config");
        let bundle = tmp.path().join("bundle");
        create_file(
            &bundle,
            ".ai/config/test.yaml",
            &lillux::signature::sign_content("name: bundle\n", &project_key, "#", None),
        );
        let signers = vec![signer_entry(
            &project_key,
            SignerScope {
                spaces: vec![SignerSpace::Project],
                kinds: vec!["*".to_string()],
                projects: Vec::new(),
            },
        )];
        let loader = VerifiedLoader::new_with_node_config(
            project.clone(),
            Some(node_config.clone()),
            vec![bundle],
            &signers,
        )
        .unwrap();
        let error = loader
            .load_config_strict_signed_with_proof::<serde_yaml::Value>("test")
            .unwrap_err();
        assert!(error.to_string().contains("signer_out_of_scope"));

        create_file(
            &node_config,
            ".ai/config/node-only.yaml",
            &lillux::signature::sign_content("name: node\n", &project_key, "#", None),
        );
        let error = loader
            .load_config_strict_signed_with_proof::<serde_yaml::Value>("node-only")
            .unwrap_err();
        assert!(error.to_string().contains("signer_out_of_scope"));

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
    fn project_scope_matches_the_logical_project_not_the_checkout() {
        let key = SigningKey::from_bytes(&[53u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        // The loader reads a materialized checkout; the registered project
        // the signer is scoped to is a different path.
        let checkout = tmp.path().join("checkout");
        let logical = tmp.path().join("registered-project");
        create_file(
            &checkout,
            ".ai/config/scoped.yaml",
            &lillux::signature::sign_content("name: scoped\n", &key, "#", None),
        );
        let signers = vec![signer_entry(
            &key,
            SignerScope {
                spaces: vec![SignerSpace::Project],
                kinds: vec!["*".to_string()],
                projects: vec![logical.to_string_lossy().into_owned()],
            },
        )];

        // Without a stated logical project the checkout path is judged, which
        // is not the scoped project: the signer is out of scope.
        let by_checkout = VerifiedLoader::new(checkout.clone(), vec![], &signers).unwrap();
        assert!(
            by_checkout
                .load_config_strict_signed_with_proof::<serde_yaml::Value>("scoped")
                .is_err()
        );

        // Stating the logical project admits the signer.
        let by_logical = VerifiedLoader::new(checkout.clone(), vec![], &signers)
            .unwrap()
            .with_logical_project_root(Some(logical.clone()));
        let loaded = by_logical
            .load_config_strict_signed_with_proof::<serde_yaml::Value>("scoped")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.value["name"], "scoped");

        // An unknown logical project admits nothing a project scope names.
        let unknown = VerifiedLoader::new(checkout, vec![], &signers)
            .unwrap()
            .with_logical_project_root(None);
        assert!(
            unknown
                .load_config_strict_signed_with_proof::<serde_yaml::Value>("scoped")
                .is_err()
        );
    }

    #[test]
    fn signer_entry_fingerprint_must_match_its_key() {
        let signing_key = SigningKey::from_bytes(&[52u8; 32]);
        let mut entry = signer_entry(&signing_key, SignerScope::everywhere());
        entry.fingerprint = "f".repeat(64);

        let error = VerifiedLoader::new(PathBuf::from("/project"), vec![], &[entry])
            .err()
            .expect("a mismatched fingerprint must be refused");
        assert!(format!("{error:#}").contains("fingerprint"));
    }

    #[test]
    fn runtime_trust_comes_from_envelope_signers() {
        let signing_key = SigningKey::from_bytes(&[99u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let system = tmp.path().join("system");
        let config_id = "ryeos-runtime/model-providers/test";
        create_file(
            &system,
            &format!(".ai/config/{config_id}.yaml"),
            &lillux::signature::sign_content(
                "base_url: https://example.com/v1\n",
                &signing_key,
                "#",
                None,
            ),
        );

        let nobody =
            VerifiedLoader::new(project.clone(), vec![system.clone()], &no_signers()).unwrap();
        assert!(nobody.trust_store().is_empty());
        assert!(
            nobody
                .load_config_strict_signed::<serde_yaml::Value>(config_id)
                .is_err()
        );

        let signers = trusted_everywhere(&signing_key);
        let trusting = VerifiedLoader::new(project, vec![system], &signers).unwrap();
        assert_eq!(
            trusting.trusted_signers_digest(),
            trusted_signers_digest(&signers)
        );
        assert_eq!(trusting.trust_store().len(), 1);
        let value = trusting
            .load_config_strict_signed::<serde_yaml::Value>(config_id)
            .unwrap()
            .unwrap();
        assert_eq!(value["base_url"].as_str(), Some("https://example.com/v1"));
    }

    #[test]
    fn runtime_never_reads_trust_directories() {
        let signing_key = SigningKey::from_bytes(&[98u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let fingerprint = lillux::signature::compute_fingerprint(&signing_key.verifying_key());
        let planted = format!(
            "fingerprint = \"{fingerprint}\"\npem = \"ed25519:{}\"\nowner = \"planted\"\n",
            base64::engine::general_purpose::STANDARD
                .encode(signing_key.verifying_key().to_bytes())
        );
        create_file(&project, ".ai/config/keys/trusted/x.toml", &planted);
        create_file(
            &project,
            ".ai/config/test.yaml",
            &lillux::signature::sign_content("name: project\n", &signing_key, "#", None),
        );

        let loader = VerifiedLoader::new(project, vec![], &no_signers()).unwrap();
        assert!(loader.trust_store().is_empty());
        assert_eq!(
            loader.trusted_signers_digest(),
            trusted_signers_digest(&no_signers())
        );
        let error = loader
            .load_config_strict_signed::<serde_yaml::Value>("test")
            .unwrap_err();
        assert!(error.to_string().contains("unknown signer"));
    }

    #[test]
    fn load_verified_strips_signature_and_hashes() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let tmp = tempfile::tempdir().unwrap();

        let body = "# Hello\n\nBody text.\n";
        let signed = sign_md(body, &sk);
        let path = tmp.path().join("test.md");
        fs::write(&path, &signed).unwrap();

        let loader =
            VerifiedLoader::new(tmp.path().to_path_buf(), vec![], &trusted_everywhere(&sk))
                .unwrap();
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

        let loader = VerifiedLoader::new(tmp.path().to_path_buf(), vec![], &no_signers()).unwrap();
        let verified = loader.load_verified("directive", &path).unwrap();

        assert_eq!(verified.content, content);
        assert_eq!(verified.hash.len(), 64);
    }

    #[test]
    fn load_verified_rejects_tampered_content() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let tmp = tempfile::tempdir().unwrap();

        let body = "# Original\n";
        let signed = sign_md(body, &sk);
        let tampered = signed.replace("# Original", "# Tampered");
        let path = tmp.path().join("tampered.md");
        fs::write(&path, &tampered).unwrap();

        let loader =
            VerifiedLoader::new(tmp.path().to_path_buf(), vec![], &trusted_everywhere(&sk))
                .unwrap();
        let result = loader.load_verified("directive", &path);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("content hash mismatch")
        );
    }

    #[test]
    fn load_verified_rejects_bad_signature() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let other_sk = SigningKey::from_bytes(&[99u8; 32]);
        let tmp = tempfile::tempdir().unwrap();

        let body = "# Test\n";
        let signed = sign_md(body, &sk);
        let sk_fp = lillux::signature::compute_fingerprint(&sk.verifying_key());
        let other_fp = lillux::signature::compute_fingerprint(&other_sk.verifying_key());
        let forged = signed.replace(&sk_fp, &other_fp);
        let path = tmp.path().join("bad_sig.md");
        fs::write(&path, &forged).unwrap();

        let loader = VerifiedLoader::new(
            tmp.path().to_path_buf(),
            vec![],
            &trusted_everywhere(&other_sk),
        )
        .unwrap();
        let result = loader.load_verified("directive", &path);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("signature verification failed")
        );
    }

    #[test]
    fn load_verified_accepts_unknown_signer() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let tmp = tempfile::tempdir().unwrap();
        let body = "# Test\n";
        let signed = sign_md(body, &sk);
        let path = tmp.path().join("unknown_signer.md");
        fs::write(&path, &signed).unwrap();

        let loader = VerifiedLoader::new(tmp.path().to_path_buf(), vec![], &no_signers()).unwrap();
        let verified = loader.load_verified("directive", &path).unwrap();

        assert!(verified.content.contains("# Test"));
    }

    #[test]
    fn load_verified_hash_is_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("det.md");
        let content = "deterministic content";
        fs::write(&path, content).unwrap();

        let loader = VerifiedLoader::new(tmp.path().to_path_buf(), vec![], &no_signers()).unwrap();
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
        let loader = VerifiedLoader::new(project, vec![bundle], &no_signers()).unwrap();
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

        let loader = VerifiedLoader::new(project, vec![], &no_signers()).unwrap();
        let items = loader.scan_kind("directive").unwrap();

        assert!(items.is_empty());
    }

    // ── Strict mode tests ──────────────────────────────────────────────

    /// Signing key used by the strict-mode tests.
    fn strict_test_key() -> SigningKey {
        SigningKey::from_bytes(&[99u8; 32])
    }

    /// Helper: sign YAML with the strict-mode test key.
    fn sign_with_test_key(yaml_body: &str) -> String {
        lillux::signature::sign_content_at(
            yaml_body,
            &strict_test_key(),
            "#",
            None,
            "2026-01-01T00:00:00Z",
        )
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

        let loader =
            VerifiedLoader::new(tmp.path().join("project"), vec![system], &no_signers()).unwrap();
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

        let loader =
            VerifiedLoader::new(tmp.path().join("project"), vec![system], &no_signers()).unwrap();
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
    fn strict_load_accepts_envelope_trusted_signed_config() {
        let tmp = tempfile::tempdir().unwrap();
        let system = tmp.path().join("system");
        let cfg_subpath = ".ai/config/ryeos-runtime/model-providers/test.yaml";
        std::fs::create_dir_all(system.join(cfg_subpath).parent().unwrap()).unwrap();

        let yaml_body = "base_url: https://example.com/v1\n";
        let signed = sign_with_test_key(yaml_body);
        std::fs::write(system.join(cfg_subpath), signed).unwrap();

        let loader = VerifiedLoader::new(
            tmp.path().join("project"),
            vec![system],
            &trusted_everywhere(&strict_test_key()),
        )
        .unwrap();
        let res = loader
            .load_config_strict_signed::<serde_yaml::Value>("ryeos-runtime/model-providers/test");
        assert!(
            res.is_ok(),
            "strict mode must accept config signed by an envelope-trusted key"
        );
        let val = res.unwrap().expect("should have a value");
        assert_eq!(val["base_url"].as_str(), Some("https://example.com/v1"));
    }

    #[test]
    fn strict_load_rejects_bundle_planted_key_document() {
        // A key document planted inside the bundle vouches for nothing.
        // Strict verification must treat the signer as unknown.
        let tmp = tempfile::tempdir().unwrap();
        let system = tmp.path().join("system");
        let cfg_subpath = ".ai/config/ryeos-runtime/model-providers/test.yaml";
        std::fs::create_dir_all(system.join(cfg_subpath).parent().unwrap()).unwrap();

        let yaml_body = "base_url: https://example.com/v1\n";
        let signed = sign_with_test_key(yaml_body);
        std::fs::write(system.join(cfg_subpath), signed).unwrap();
        create_file(
            &system,
            ".ai/config/keys/trusted/test.toml",
            "owner = \"planted\"\n",
        );

        let loader =
            VerifiedLoader::new(tmp.path().join("project"), vec![system], &no_signers()).unwrap();
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
