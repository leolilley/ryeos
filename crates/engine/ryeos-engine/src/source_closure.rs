//! Admission mechanics for executable source adjacent to a signed item.
//!
//! This module is meaning-blind about tools, workers, Python, and projects.
//! It consumes an already-authorized logical file view, builds the exact
//! content-only manifest, and produces a proof that the binding can be
//! redeemed from retained storage. Filesystem/root acquisition and CAS
//! publication remain daemon responsibilities.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::EngineError;
use crate::kind_registry::ExecutionSourceClosureDecl;
use crate::project_content::ProjectContentEntry;

pub use ryeos_state::objects::{
    EFFECTIVE_SOURCE_BINDING_KIND, EFFECTIVE_SOURCE_BINDING_SCHEMA, EffectiveSourceBinding,
    EffectiveSourceClosureProjection, LogicalSourceRoot, SOURCE_CLOSURE_DERIVED_KEY,
    SOURCE_CLOSURE_MANIFEST_KIND, SOURCE_CLOSURE_MANIFEST_SCHEMA, SignedKindSourceCeiling,
    SourceClosureFile, SourceClosureManifest, SourceExecutionPolicyIdentity, SourceFileMode,
    SourceLoaderRoot, SourceLogicalBinding, SourceOwnerIdentity, SourceRootIdentity,
    SourceSpaceIdentity, SourceTestimonyProof,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSourceDeclaration {
    pub root: String,
    pub entry: String,
    pub digest: String,
}

impl WorkerSourceDeclaration {
    pub fn from_composed(
        composed: &serde_json::Value,
        contract: Option<&ExecutionSourceClosureDecl>,
    ) -> anyhow::Result<Option<Self>> {
        let authored = composed.get("source");
        let Some(contract) = contract else {
            if authored.is_some() {
                anyhow::bail!(
                    "item declares `source` but its signed kind has no execution.source_closure contract"
                );
            }
            return Ok(None);
        };
        if !matches!(
            contract.location,
            crate::kind_registry::SourceClosureLocationDecl::OwnerRelativeSource { .. }
        ) {
            if authored.is_some() {
                anyhow::bail!("item source declaration is incompatible with its signed kind");
            }
            return Ok(None);
        }
        let value = authored.ok_or_else(|| {
            anyhow::anyhow!("source-owning item is missing its required source declaration")
        })?;
        let declaration: Self = serde_json::from_value(value.clone())
            .map_err(|error| anyhow::anyhow!("invalid source declaration: {error}"))?;
        declaration.validate()?;
        Ok(Some(declaration))
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        validate_relative_path("source root", &self.root)?;
        validate_relative_path("source entry", &self.entry)?;
        ryeos_state::objects::thread_snapshot::validate_canonical_hash(
            "source declaration digest",
            &self.digest,
        )?;
        Ok(())
    }

    pub fn identity_digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        let value = serde_json::to_value(self)?;
        let canonical = lillux::canonical_json(&value)?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorSourcePolicy {
    pub location: ExecutorSourceLocation,
    pub load_roots: Vec<SourceLoaderRoot>,
    pub materialization: SourceMaterialization,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorSourceLocation {
    ItemNamespace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceMaterialization {
    ReadOnly,
}

impl ExecutorSourcePolicy {
    pub fn from_value(value: &serde_json::Value) -> anyhow::Result<Self> {
        let policy: Self = serde_json::from_value(value.clone())?;
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.load_roots.is_empty() || self.load_roots.len() > 3 {
            anyhow::bail!("source_scope load_roots must contain 1..=3 closed roots");
        }
        let mut unique = std::collections::BTreeSet::new();
        for root in &self.load_roots {
            if !unique.insert(root) {
                anyhow::bail!("source_scope load_roots contains a duplicate");
            }
        }
        Ok(())
    }

    pub fn digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        let canonical = lillux::canonical_json(&serde_json::to_value(self)?)?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRootRequest {
    pub id: String,
    pub selection: SourceRootSelection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceRootSelection {
    Tree { prefix: PathBuf },
    File { path: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedSourceBlob {
    pub blob_hash: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedSourceCandidate {
    pub manifest: SourceClosureManifest,
    pub blobs: Vec<CapturedSourceBlob>,
    census: Vec<SourceCensusEntry>,
    authority_identity: String,
}

impl CapturedSourceCandidate {
    pub fn authority_identity(&self) -> &str {
        &self.authority_identity
    }

    pub fn revalidate(
        &self,
        content: &dyn AuthoritativeSourceContent,
        roots: &[SourceRootRequest],
        ceiling: &ExecutionSourceClosureDecl,
    ) -> SourceAuthorityStatus {
        match census(content, roots, ceiling) {
            Ok((current, identity))
                if current == self.census && identity == self.authority_identity =>
            {
                SourceAuthorityStatus::Current
            }
            Ok(_) => SourceAuthorityStatus::MutableAuthorityChanged,
            Err(_) => SourceAuthorityStatus::Unavailable,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceAuthorityStatus {
    Current,
    MutableAuthorityChanged,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SourceCensusEntry {
    root: String,
    path: PathBuf,
    content_hash: String,
    size: u64,
    normalized_mode: u32,
}

/// Logical exact-file authority. Implementations must already apply the
/// shared durable capture floor and configured ignore policy. No method
/// returns a host root.
pub trait AuthoritativeSourceContent {
    fn authority_identity(&self) -> Result<String, EngineError>;

    fn list_files(
        &self,
        prefix: &Path,
        max_entries: usize,
    ) -> Result<Vec<ProjectContentEntry>, EngineError>;

    fn read_file(&self, path: &Path, max_bytes: u64) -> Result<Option<Vec<u8>>, EngineError>;
}

pub fn capture_source_candidate(
    content: &dyn AuthoritativeSourceContent,
    roots: &[SourceRootRequest],
    ceiling: &ExecutionSourceClosureDecl,
) -> Result<CapturedSourceCandidate, EngineError> {
    let (observed, authority_identity) = census(content, roots, ceiling)?;
    let mut entries = Vec::with_capacity(observed.len());
    let mut blobs = Vec::with_capacity(observed.len());
    for observed in &observed {
        let request = roots
            .iter()
            .find(|root| root.id == observed.root)
            .expect("census root came from request");
        let full_path = match &request.selection {
            SourceRootSelection::Tree { prefix } => prefix.join(&observed.path),
            SourceRootSelection::File { path } => path.clone(),
        };
        let bytes = content
            .read_file(&full_path, ceiling.max_file_bytes)?
            .ok_or(EngineError::MutableEffectiveProgramAuthorityChanged)?;
        let actual = lillux::sha256_hex(&bytes);
        if actual != observed.content_hash || bytes.len() as u64 != observed.size {
            return Err(EngineError::MutableEffectiveProgramAuthorityChanged);
        }
        let mode = match observed.normalized_mode {
            0o644 => SourceFileMode::ReadOnly,
            0o755 => SourceFileMode::Executable,
            other => {
                return Err(EngineError::Internal(format!(
                    "authoritative source returned unsupported normalized mode {other:o}"
                )));
            }
        };
        entries.push(SourceClosureFile {
            root: observed.root.clone(),
            path: path_string(&observed.path)?,
            blob_hash: actual.clone(),
            size: bytes.len() as u64,
            mode,
        });
        blobs.push(CapturedSourceBlob {
            blob_hash: actual,
            bytes,
        });
    }
    let manifest = SourceClosureManifest::new(
        roots
            .iter()
            .map(|root| LogicalSourceRoot {
                id: root.id.clone(),
            })
            .collect(),
        entries,
    )
    .map_err(|error| EngineError::Internal(error.to_string()))?;
    let (after, after_identity) = census(content, roots, ceiling)?;
    if observed != after || authority_identity != after_identity {
        return Err(EngineError::MutableEffectiveProgramAuthorityChanged);
    }
    Ok(CapturedSourceCandidate {
        manifest,
        blobs,
        census: observed,
        authority_identity,
    })
}

fn census(
    content: &dyn AuthoritativeSourceContent,
    roots: &[SourceRootRequest],
    ceiling: &ExecutionSourceClosureDecl,
) -> Result<(Vec<SourceCensusEntry>, String), EngineError> {
    if roots.is_empty() || roots.len() > ryeos_state::objects::MAX_SOURCE_ROOTS {
        return Err(EngineError::Internal(
            "source closure has an invalid logical root count".to_owned(),
        ));
    }
    let mut remaining = ceiling.max_files;
    let mut census = Vec::new();
    let mut total_bytes = 0u64;
    for root in roots {
        validate_root_id(&root.id)?;
        if remaining == 0 {
            return Err(EngineError::Internal(
                "source closure exceeds its signed file ceiling".to_owned(),
            ));
        }
        let listed = match &root.selection {
            SourceRootSelection::Tree { prefix } => content.list_files(prefix, remaining)?,
            SourceRootSelection::File { path } => {
                let parent = path.parent().unwrap_or_else(|| Path::new(""));
                let name = path.file_name().ok_or_else(|| {
                    EngineError::Internal("source file selection has no file name".to_owned())
                })?;
                let selected = content
                    .list_files(parent, ryeos_state::objects::MAX_SOURCE_FILES)?
                    .into_iter()
                    .find(|entry| entry.relative_path == Path::new(name))
                    .ok_or_else(|| {
                        EngineError::Internal("source root entry is absent".to_owned())
                    })?;
                vec![selected]
            }
        };
        remaining = remaining.checked_sub(listed.len()).ok_or_else(|| {
            EngineError::Internal("source closure file count overflow".to_owned())
        })?;
        for entry in listed {
            if entry.size > ceiling.max_file_bytes {
                return Err(EngineError::Internal(
                    "source closure file exceeds its signed byte ceiling".to_owned(),
                ));
            }
            let depth = entry.relative_path.components().count();
            if depth == 0 || depth > ceiling.max_depth {
                return Err(EngineError::Internal(
                    "source closure path exceeds its signed depth ceiling".to_owned(),
                ));
            }
            total_bytes = total_bytes.checked_add(entry.size).ok_or_else(|| {
                EngineError::Internal("source closure byte count overflow".to_owned())
            })?;
            if total_bytes > ceiling.max_total_bytes {
                return Err(EngineError::Internal(
                    "source closure exceeds its signed aggregate byte ceiling".to_owned(),
                ));
            }
            census.push(SourceCensusEntry {
                root: root.id.clone(),
                path: entry.relative_path,
                content_hash: entry.content_hash,
                size: entry.size,
                normalized_mode: entry.normalized_mode,
            });
        }
    }
    census.sort();
    if census.is_empty() {
        return Err(EngineError::Internal(
            "source closure contains no admitted regular files".to_owned(),
        ));
    }
    Ok((census, content.authority_identity()?))
}

fn validate_relative_path(label: &str, value: &str) -> anyhow::Result<()> {
    ryeos_state::objects::validate_canonical_project_relative_path(value)
        .map_err(|error| anyhow::anyhow!("{label}: {error}"))
}

fn validate_root_id(value: &str) -> Result<(), EngineError> {
    if value.is_empty()
        || value.len() > 128
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b'/')
    {
        return Err(EngineError::Internal(
            "source closure root id is not canonical".to_owned(),
        ));
    }
    Ok(())
}

fn path_string(path: &Path) -> Result<String, EngineError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| EngineError::Internal("source closure contains a non-UTF-8 path".to_owned()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceClosureProofStatus {
    Current,
    Unavailable,
}

#[derive(Debug, Clone)]
pub struct SourceClosureProof {
    projection: EffectiveSourceClosureProjection,
}

impl SourceClosureProof {
    pub fn projection(&self) -> &EffectiveSourceClosureProjection {
        &self.projection
    }

    pub fn identity_digest(&self) -> Result<String, EngineError> {
        let value = self
            .projection
            .to_value()
            .map_err(|error| EngineError::Internal(error.to_string()))?;
        let canonical = lillux::canonical_json(&value)
            .map_err(|error| EngineError::Internal(error.to_string()))?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }

    pub(crate) fn revalidate(&self, store: &dyn SourceClosureStore) -> SourceClosureProofStatus {
        match store.source_closure_available(
            &self.projection.binding_hash,
            &self.projection.content_manifest_hash,
        ) {
            Ok(true) => SourceClosureProofStatus::Current,
            Ok(false) | Err(_) => SourceClosureProofStatus::Unavailable,
        }
    }
}

pub trait SourceClosureStore {
    fn source_closure_available(
        &self,
        binding_hash: &str,
        manifest_hash: &str,
    ) -> anyhow::Result<bool>;
}

pub fn prove_source_closure(
    projection: EffectiveSourceClosureProjection,
    store: &dyn SourceClosureStore,
) -> Result<SourceClosureProof, EngineError> {
    projection
        .validate()
        .map_err(|error| EngineError::Internal(error.to_string()))?;
    let proof = SourceClosureProof { projection };
    if proof.revalidate(store) != SourceClosureProofStatus::Current {
        return Err(EngineError::Internal(
            "admitted source closure is unavailable from verified CAS authority".to_owned(),
        ));
    }
    Ok(proof)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::kind_registry::{SourceClosureLocationDecl, SourceClosureTestimonyDecl};

    struct MemorySource {
        files: BTreeMap<PathBuf, Vec<u8>>,
    }

    impl AuthoritativeSourceContent for MemorySource {
        fn authority_identity(&self) -> Result<String, EngineError> {
            Ok("memory-authority".to_owned())
        }

        fn list_files(
            &self,
            prefix: &Path,
            max_entries: usize,
        ) -> Result<Vec<ProjectContentEntry>, EngineError> {
            let mut entries = self
                .files
                .iter()
                .filter_map(|(path, bytes)| {
                    let relative = path.strip_prefix(prefix).ok()?;
                    Some(ProjectContentEntry {
                        relative_path: relative.to_path_buf(),
                        content_hash: lillux::sha256_hex(bytes),
                        size: bytes.len() as u64,
                        normalized_mode: 0o644,
                    })
                })
                .collect::<Vec<_>>();
            entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
            if entries.len() > max_entries {
                return Err(EngineError::Internal("too many source files".to_owned()));
            }
            Ok(entries)
        }

        fn read_file(&self, path: &Path, _max_bytes: u64) -> Result<Option<Vec<u8>>, EngineError> {
            Ok(self.files.get(path).cloned())
        }
    }

    fn ceiling() -> ExecutionSourceClosureDecl {
        ExecutionSourceClosureDecl {
            derived: SOURCE_CLOSURE_DERIVED_KEY.to_owned(),
            location: SourceClosureLocationDecl::ItemNamespace,
            testimony: SourceClosureTestimonyDecl::OwnerSignedFiles,
            max_files: 8,
            max_total_bytes: 1024,
            max_file_bytes: 512,
            max_depth: 8,
        }
    }

    #[test]
    fn capture_is_path_independent_and_double_censused() {
        let first = MemorySource {
            files: BTreeMap::from([
                (PathBuf::from("install-a/main.py"), b"main".to_vec()),
                (PathBuf::from("install-a/lib/helper.py"), b"helper".to_vec()),
            ]),
        };
        let second = MemorySource {
            files: BTreeMap::from([
                (PathBuf::from("install-b/main.py"), b"main".to_vec()),
                (PathBuf::from("install-b/lib/helper.py"), b"helper".to_vec()),
            ]),
        };
        let first = capture_source_candidate(
            &first,
            &[SourceRootRequest {
                id: "source".to_owned(),
                selection: SourceRootSelection::Tree {
                    prefix: PathBuf::from("install-a"),
                },
            }],
            &ceiling(),
        )
        .unwrap();
        let second = capture_source_candidate(
            &second,
            &[SourceRootRequest {
                id: "source".to_owned(),
                selection: SourceRootSelection::Tree {
                    prefix: PathBuf::from("install-b"),
                },
            }],
            &ceiling(),
        )
        .unwrap();
        assert_eq!(
            first.manifest.digest().unwrap(),
            second.manifest.digest().unwrap()
        );
    }

    #[test]
    fn worker_source_is_atomic_and_entry_is_beneath_root() {
        let declaration: WorkerSourceDeclaration = serde_json::from_value(serde_json::json!({
            "root": "lib/session",
            "entry": "bootstrap.py",
            "digest": "a".repeat(64)
        }))
        .unwrap();
        declaration.validate().unwrap();
        let mut escaped = declaration;
        escaped.entry = "../bootstrap.py".to_owned();
        assert!(escaped.validate().is_err());
    }
}
