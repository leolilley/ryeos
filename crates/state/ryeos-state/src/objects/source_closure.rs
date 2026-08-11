//! Durable content and authority records for admitted executable source.
//!
//! The manifest is deliberately content-only.  The binding is the separate
//! testimony record which says whose source it is, which signed kind ceiling
//! admitted it, and how an executor may present it to a process.  Keeping the
//! two objects separate lets equal source trees share blobs without collapsing
//! distinct publishers or execution policies into one identity.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SOURCE_CLOSURE_DERIVED_KEY: &str = "effective_source_closure";
pub const SOURCE_CLOSURE_MANIFEST_KIND: &str = "ryeos.source_closure_manifest";
pub const SOURCE_CLOSURE_MANIFEST_SCHEMA: u32 = 1;
pub const EFFECTIVE_SOURCE_BINDING_KIND: &str = "ryeos.effective_source_binding";
pub const EFFECTIVE_SOURCE_BINDING_SCHEMA: u32 = 1;

pub const MAX_SOURCE_ROOTS: usize = 8;
pub const MAX_SOURCE_FILES: usize = 4096;
pub const MAX_SOURCE_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_SOURCE_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_SOURCE_PATH_BYTES: usize = 4096;
pub const MAX_SOURCE_MANIFEST_BYTES: usize = 1024 * 1024;
pub const MAX_SOURCE_BINDING_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalSourceRoot {
    /// Manifest-local identifier. It is never an item, project, bundle, or
    /// host path.
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFileMode {
    ReadOnly,
    Executable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceClosureFile {
    pub root: String,
    pub path: String,
    pub blob_hash: String,
    pub size: u64,
    pub mode: SourceFileMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceClosureTotals {
    pub file_count: usize,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceClosureManifest {
    pub schema: u32,
    pub kind: String,
    pub roots: Vec<LogicalSourceRoot>,
    pub entries: Vec<SourceClosureFile>,
    pub totals: SourceClosureTotals,
}

impl SourceClosureManifest {
    pub fn new(
        mut roots: Vec<LogicalSourceRoot>,
        mut entries: Vec<SourceClosureFile>,
    ) -> anyhow::Result<Self> {
        roots.sort_by(|left, right| left.id.as_bytes().cmp(right.id.as_bytes()));
        entries.sort_by(|left, right| {
            (left.root.as_bytes(), left.path.as_bytes())
                .cmp(&(right.root.as_bytes(), right.path.as_bytes()))
        });
        let total_bytes = entries.iter().try_fold(0u64, |total, entry| {
            total
                .checked_add(entry.size)
                .ok_or_else(|| anyhow::anyhow!("source closure byte count overflow"))
        })?;
        let manifest = Self {
            schema: SOURCE_CLOSURE_MANIFEST_SCHEMA,
            kind: SOURCE_CLOSURE_MANIFEST_KIND.to_owned(),
            totals: SourceClosureTotals {
                file_count: entries.len(),
                total_bytes,
            },
            roots,
            entries,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let manifest: Self = serde_json::from_value(value.clone())?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn digest(&self) -> anyhow::Result<String> {
        let canonical = lillux::canonical_json(&self.to_value()?)?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }

    pub fn blob_hashes(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.blob_hash.clone())
            .collect()
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != SOURCE_CLOSURE_MANIFEST_SCHEMA {
            anyhow::bail!("source closure manifest schema is not current");
        }
        if self.kind != SOURCE_CLOSURE_MANIFEST_KIND {
            anyhow::bail!("source closure manifest kind is invalid");
        }
        if self.roots.is_empty() || self.roots.len() > MAX_SOURCE_ROOTS {
            anyhow::bail!("source closure manifest has an invalid root count");
        }
        if self.entries.is_empty() || self.entries.len() > MAX_SOURCE_FILES {
            anyhow::bail!("source closure manifest has an invalid file count");
        }
        if self.totals.file_count != self.entries.len() {
            anyhow::bail!("source closure file count contradicts its entries");
        }

        let mut root_ids = BTreeSet::new();
        let mut previous_root: Option<&str> = None;
        for root in &self.roots {
            validate_key("source closure root", &root.id, 128)?;
            if let Some(previous) = previous_root
                && previous.as_bytes() >= root.id.as_bytes()
            {
                anyhow::bail!("source closure roots are not strictly bytewise ordered");
            }
            previous_root = Some(&root.id);
            root_ids.insert(root.id.as_str());
        }

        let mut previous_entry: Option<(&str, &str)> = None;
        let mut paths = BTreeSet::new();
        let mut total_bytes = 0u64;
        for entry in &self.entries {
            if !root_ids.contains(entry.root.as_str()) {
                anyhow::bail!("source closure entry names an undeclared root");
            }
            super::validate_canonical_project_relative_path(&entry.path)?;
            if entry.path.len() > MAX_SOURCE_PATH_BYTES {
                anyhow::bail!("source closure path exceeds the byte bound");
            }
            super::thread_snapshot::validate_canonical_hash(
                "source closure blob hash",
                &entry.blob_hash,
            )?;
            if entry.size > MAX_SOURCE_FILE_BYTES {
                anyhow::bail!("source closure file exceeds the byte bound");
            }
            let key = (entry.root.as_str(), entry.path.as_str());
            if let Some(previous) = previous_entry
                && (previous.0.as_bytes(), previous.1.as_bytes())
                    >= (key.0.as_bytes(), key.1.as_bytes())
            {
                anyhow::bail!("source closure entries are not strictly bytewise ordered");
            }
            previous_entry = Some(key);
            if !paths.insert(key) {
                anyhow::bail!("source closure contains a duplicate file");
            }
            total_bytes = total_bytes
                .checked_add(entry.size)
                .ok_or_else(|| anyhow::anyhow!("source closure byte count overflow"))?;
        }
        if total_bytes != self.totals.total_bytes {
            anyhow::bail!("source closure total bytes contradict its entries");
        }
        if total_bytes > MAX_SOURCE_TOTAL_BYTES {
            anyhow::bail!("source closure exceeds the aggregate byte bound");
        }

        // A regular file cannot also be the ancestor of another regular file.
        for (root, path) in &paths {
            let mut cursor = std::path::Path::new(path).parent();
            while let Some(parent) = cursor {
                if !parent.as_os_str().is_empty() {
                    let parent = parent
                        .to_str()
                        .ok_or_else(|| anyhow::anyhow!("source closure path is not UTF-8"))?;
                    if paths.contains(&(*root, parent)) {
                        anyhow::bail!("source closure file is an ancestor of another file");
                    }
                }
                cursor = parent.parent();
            }
        }

        let canonical = lillux::canonical_json(&serde_json::to_value(self)?)?;
        if canonical.len() > MAX_SOURCE_MANIFEST_BYTES {
            anyhow::bail!("source closure manifest exceeds the serialized byte bound");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceSpaceIdentity {
    Project,
    Bundle,
    Node,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceRootIdentity {
    Project,
    Bundle { name: String },
    Node,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceOwnerIdentity {
    pub canonical_ref: String,
    pub item_kind: String,
    pub source_space: SourceSpaceIdentity,
    pub source_root: SourceRootIdentity,
    pub root_source_content_digest: String,
    pub root_raw_content_digest: String,
    pub signer_fingerprint: String,
    pub logical_item_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedKindSourceCeiling {
    pub schema_ref: String,
    pub source_content_digest: String,
    pub raw_content_digest: String,
    pub signer_fingerprint: String,
    pub signature_header: String,
    pub schema_body: String,
    pub schema_document: Value,
    pub normalized_declaration: Value,
    pub root_kind_format: Value,
    pub root_signature_envelope: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceTestimonyProof {
    OwnerSignedFiles {
        signer_fingerprint: String,
        file_count: usize,
        entries_digest: String,
    },
    OwnerSignedDigest {
        expected_manifest_hash: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceExecutionPolicyIdentity {
    Executor {
        declarer_ref: String,
        signer_fingerprint: String,
        source_content_digest: String,
        raw_content_digest: String,
        policy_digest: String,
        chain_digest: String,
    },
    Worker {
        source_declaration_digest: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceLoaderRoot {
    ItemDirectory,
    NamespaceRoot,
    NamespaceLib,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceLogicalBinding {
    Tool {
        loader_roots: Vec<SourceLoaderRoot>,
        root_entry: String,
    },
    Worker {
        root: String,
        entry: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveSourceBinding {
    pub schema: u32,
    pub kind: String,
    pub owner: SourceOwnerIdentity,
    pub kind_ceiling: SignedKindSourceCeiling,
    pub content_manifest_hash: String,
    pub testimony: SourceTestimonyProof,
    pub execution_policy: SourceExecutionPolicyIdentity,
    pub logical_binding: SourceLogicalBinding,
}

/// Bounded daemon-owned projection placed in the effective composed view.
/// The full authority remains in the CAS binding object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveSourceClosureProjection {
    pub schema: u32,
    pub binding_hash: String,
    pub content_manifest_hash: String,
    pub owner_key: String,
    pub file_count: usize,
    pub total_bytes: u64,
}

impl EffectiveSourceClosureProjection {
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let projection: Self = serde_json::from_value(value.clone())?;
        projection.validate()?;
        Ok(projection)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != EFFECTIVE_SOURCE_BINDING_SCHEMA {
            anyhow::bail!("effective source projection schema is not current");
        }
        validate_hashes([
            ("effective source binding", &self.binding_hash),
            (
                "effective source content manifest",
                &self.content_manifest_hash,
            ),
            ("effective source owner key", &self.owner_key),
        ])?;
        if self.file_count == 0 || self.file_count > MAX_SOURCE_FILES {
            anyhow::bail!("effective source projection file count is invalid");
        }
        if self.total_bytes > MAX_SOURCE_TOTAL_BYTES {
            anyhow::bail!("effective source projection byte count is invalid");
        }
        Ok(())
    }
}

impl EffectiveSourceBinding {
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let binding: Self = serde_json::from_value(value.clone())?;
        binding.validate()?;
        Ok(binding)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn digest(&self) -> anyhow::Result<String> {
        let canonical = lillux::canonical_json(&self.to_value()?)?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }

    pub fn owner_key(&self) -> anyhow::Result<String> {
        let value = serde_json::to_value(&self.owner)?;
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&value)?.as_bytes(),
        ))
    }

    pub fn validate_content_manifest(
        &self,
        manifest: &SourceClosureManifest,
    ) -> anyhow::Result<()> {
        self.validate()?;
        manifest.validate()?;
        if manifest.digest()? != self.content_manifest_hash {
            anyhow::bail!("source manifest contradicts its authority binding");
        }
        match &self.testimony {
            SourceTestimonyProof::OwnerSignedFiles { file_count, .. }
                if *file_count != manifest.entries.len() =>
            {
                anyhow::bail!("source file testimony count contradicts its manifest");
            }
            SourceTestimonyProof::OwnerSignedDigest {
                expected_manifest_hash,
            } if expected_manifest_hash != &self.content_manifest_hash => {
                anyhow::bail!("source digest testimony contradicts its manifest");
            }
            _ => {}
        }
        let entry = match &self.logical_binding {
            SourceLogicalBinding::Tool { root_entry, .. } => root_entry,
            SourceLogicalBinding::Worker { entry, .. } => entry,
        };
        if !manifest
            .entries
            .iter()
            .any(|file| file.root == "source" && file.path.as_str() == entry)
        {
            anyhow::bail!("source manifest does not contain its admitted entry");
        }

        let declaration = self
            .kind_ceiling
            .normalized_declaration
            .as_object()
            .expect("binding validation proved a declaration object");
        let limit = |name: &str| -> anyhow::Result<u64> {
            declaration
                .get(name)
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow::anyhow!("source kind declaration has no `{name}` limit"))
        };
        let max_files = limit("max_files")?;
        let max_total_bytes = limit("max_total_bytes")?;
        let max_file_bytes = limit("max_file_bytes")?;
        let max_depth = limit("max_depth")?;
        if manifest.entries.len() as u64 > max_files
            || manifest.totals.total_bytes > max_total_bytes
            || manifest.entries.iter().any(|file| {
                file.size > max_file_bytes
                    || std::path::Path::new(&file.path).components().count() as u64 > max_depth
            })
        {
            anyhow::bail!("source manifest exceeds its signed kind ceiling");
        }
        Ok(())
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != EFFECTIVE_SOURCE_BINDING_SCHEMA {
            anyhow::bail!("effective source binding schema is not current");
        }
        if self.kind != EFFECTIVE_SOURCE_BINDING_KIND {
            anyhow::bail!("effective source binding kind is invalid");
        }
        validate_ref("source owner canonical ref", &self.owner.canonical_ref)?;
        validate_key("source owner item kind", &self.owner.item_kind, 64)?;
        validate_key(
            "source owner logical item key",
            &self.owner.logical_item_key,
            512,
        )?;
        let (canonical_kind, _) = self
            .owner
            .canonical_ref
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("source owner canonical ref is not canonical"))?;
        if canonical_kind != self.owner.item_kind {
            anyhow::bail!("source owner item kind contradicts its canonical ref");
        }
        if !matches!(
            (&self.owner.source_space, &self.owner.source_root),
            (SourceSpaceIdentity::Project, SourceRootIdentity::Project)
                | (
                    SourceSpaceIdentity::Bundle,
                    SourceRootIdentity::Bundle { .. }
                )
                | (SourceSpaceIdentity::Node, SourceRootIdentity::Node)
        ) {
            anyhow::bail!("source owner space contradicts its typed source root");
        }
        validate_hashes([
            (
                "source owner source-content digest",
                &self.owner.root_source_content_digest,
            ),
            (
                "source owner raw-content digest",
                &self.owner.root_raw_content_digest,
            ),
            ("source owner signer", &self.owner.signer_fingerprint),
            (
                "source kind source-content digest",
                &self.kind_ceiling.source_content_digest,
            ),
            (
                "source kind raw-content digest",
                &self.kind_ceiling.raw_content_digest,
            ),
            ("source kind signer", &self.kind_ceiling.signer_fingerprint),
            ("source content manifest", &self.content_manifest_hash),
        ])?;
        validate_ref("source kind schema ref", &self.kind_ceiling.schema_ref)?;
        validate_key(
            "source kind signature header",
            &self.kind_ceiling.signature_header,
            8192,
        )?;
        if self.kind_ceiling.schema_body.is_empty()
            || self.kind_ceiling.schema_body.len() > 256 * 1024
        {
            anyhow::bail!("source kind signed body exceeds its retained ceiling");
        }
        let body_digest = lillux::signature::content_hash(&self.kind_ceiling.schema_body);
        if body_digest != self.kind_ceiling.raw_content_digest {
            anyhow::bail!("source kind signed body contradicts its raw-content digest");
        }
        for (label, value) in [
            (
                "source kind schema document",
                &self.kind_ceiling.schema_document,
            ),
            (
                "source kind normalized declaration",
                &self.kind_ceiling.normalized_declaration,
            ),
            (
                "source kind root format",
                &self.kind_ceiling.root_kind_format,
            ),
            (
                "source kind signature envelope",
                &self.kind_ceiling.root_signature_envelope,
            ),
        ] {
            validate_bounded_object(label, value, 256 * 1024)?;
        }
        match &self.owner.source_root {
            SourceRootIdentity::Project | SourceRootIdentity::Node => {}
            SourceRootIdentity::Bundle { name } => {
                validate_key("source bundle name", name, 128)?;
            }
        }
        match &self.testimony {
            SourceTestimonyProof::OwnerSignedFiles {
                signer_fingerprint,
                file_count,
                entries_digest,
            } => {
                validate_hashes([
                    ("source testimony signer", signer_fingerprint),
                    ("source testimony entries", entries_digest),
                ])?;
                if *file_count == 0 || *file_count > MAX_SOURCE_FILES {
                    anyhow::bail!("source testimony file count is invalid");
                }
                if signer_fingerprint != &self.owner.signer_fingerprint {
                    anyhow::bail!("source testimony signer differs from source owner");
                }
            }
            SourceTestimonyProof::OwnerSignedDigest {
                expected_manifest_hash,
            } => {
                super::thread_snapshot::validate_canonical_hash(
                    "source testimony expected manifest",
                    expected_manifest_hash,
                )?;
                if expected_manifest_hash != &self.content_manifest_hash {
                    anyhow::bail!("source testimony digest differs from observed content");
                }
            }
        }
        match &self.execution_policy {
            SourceExecutionPolicyIdentity::Executor {
                declarer_ref,
                signer_fingerprint,
                source_content_digest,
                raw_content_digest,
                policy_digest,
                chain_digest,
            } => {
                validate_ref("source policy declarer", declarer_ref)?;
                validate_hashes([
                    ("source policy signer", signer_fingerprint),
                    ("source policy source-content digest", source_content_digest),
                    ("source policy raw-content digest", raw_content_digest),
                    ("source policy digest", policy_digest),
                    ("source policy chain digest", chain_digest),
                ])?;
            }
            SourceExecutionPolicyIdentity::Worker {
                source_declaration_digest,
            } => {
                super::thread_snapshot::validate_canonical_hash(
                    "worker source declaration digest",
                    source_declaration_digest,
                )?;
            }
        }
        match &self.logical_binding {
            SourceLogicalBinding::Tool {
                loader_roots,
                root_entry,
            } => {
                if loader_roots.is_empty() || loader_roots.len() > 3 {
                    anyhow::bail!("tool source loader roots are invalid");
                }
                let unique = loader_roots.iter().collect::<BTreeSet<_>>();
                if unique.len() != loader_roots.len() {
                    anyhow::bail!("tool source loader roots contain a duplicate");
                }
                super::validate_canonical_project_relative_path(root_entry)?;
            }
            SourceLogicalBinding::Worker { root, entry } => {
                super::validate_canonical_project_relative_path(root)?;
                super::validate_canonical_project_relative_path(entry)?;
            }
        }
        let declaration = self
            .kind_ceiling
            .normalized_declaration
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("source kind declaration is not an object"))?;
        if declaration.get("derived").and_then(Value::as_str) != Some(SOURCE_CLOSURE_DERIVED_KEY) {
            anyhow::bail!("source kind declaration names a foreign derived slot");
        }
        let location = declaration
            .get("location")
            .and_then(Value::as_object)
            .and_then(|location| location.get("type"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("source kind declaration has no location type"))?;
        let testimony = declaration
            .get("testimony")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("source kind declaration has no testimony type"))?;
        let coherent = matches!(
            (
                location,
                testimony,
                &self.testimony,
                &self.execution_policy,
                &self.logical_binding,
            ),
            (
                "item_namespace",
                "owner_signed_files",
                SourceTestimonyProof::OwnerSignedFiles { .. },
                SourceExecutionPolicyIdentity::Executor { .. },
                SourceLogicalBinding::Tool { .. },
            ) | (
                "owner_relative_source",
                "owner_signed_digest",
                SourceTestimonyProof::OwnerSignedDigest { .. },
                SourceExecutionPolicyIdentity::Worker { .. },
                SourceLogicalBinding::Worker { .. },
            )
        );
        if !coherent {
            anyhow::bail!("source binding contradicts its signed kind source contract");
        }
        let canonical = lillux::canonical_json(&serde_json::to_value(self)?)?;
        if canonical.len() > MAX_SOURCE_BINDING_BYTES {
            anyhow::bail!("effective source binding exceeds the serialized byte bound");
        }
        Ok(())
    }
}

fn validate_hashes<'a>(
    values: impl IntoIterator<Item = (&'a str, &'a String)>,
) -> anyhow::Result<()> {
    for (label, value) in values {
        super::thread_snapshot::validate_canonical_hash(label, value)?;
    }
    Ok(())
}

fn validate_ref(label: &str, value: &str) -> anyhow::Result<()> {
    validate_key(label, value, 512)?;
    let Some((kind, id)) = value.split_once(':') else {
        anyhow::bail!("{label} is not canonical");
    };
    validate_key(label, kind, 64)?;
    validate_key(label, id, 448)
}

fn validate_key(label: &str, value: &str, max: usize) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > max
        || value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        anyhow::bail!("{label} is empty, unbounded, or non-canonical");
    }
    Ok(())
}

fn validate_bounded_object(label: &str, value: &Value, max: usize) -> anyhow::Result<()> {
    if !value.is_object() {
        anyhow::bail!("{label} is not an object");
    }
    let canonical = lillux::canonical_json(value)?;
    if canonical.len() > max {
        anyhow::bail!("{label} exceeds the serialized byte bound");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, byte: char) -> SourceClosureFile {
        SourceClosureFile {
            root: "source".to_owned(),
            path: path.to_owned(),
            blob_hash: byte.to_string().repeat(64),
            size: 1,
            mode: SourceFileMode::ReadOnly,
        }
    }

    #[test]
    fn manifest_canonicalizes_roots_and_files() {
        let left = SourceClosureManifest::new(
            vec![LogicalSourceRoot {
                id: "source".to_owned(),
            }],
            vec![file("lib/z.py", 'b'), file("main.py", 'a')],
        )
        .unwrap();
        let right = SourceClosureManifest::new(
            vec![LogicalSourceRoot {
                id: "source".to_owned(),
            }],
            vec![file("main.py", 'a'), file("lib/z.py", 'b')],
        )
        .unwrap();
        assert_eq!(left, right);
        assert_eq!(left.digest().unwrap(), right.digest().unwrap());
        assert_eq!(left.blob_hashes(), vec!["b".repeat(64), "a".repeat(64)]);
    }

    #[test]
    fn manifest_refuses_regular_file_ancestors() {
        let error = SourceClosureManifest::new(
            vec![LogicalSourceRoot {
                id: "source".to_owned(),
            }],
            vec![file("lib", 'a'), file("lib/z.py", 'b')],
        )
        .unwrap_err();
        assert!(error.to_string().contains("ancestor"));
    }

    #[test]
    fn binding_identity_separates_owner_from_shared_content() {
        let schema_body = "kind: kind\n".to_owned();
        let schema_body_digest = lillux::signature::content_hash(&schema_body);
        let binding = EffectiveSourceBinding {
            schema: EFFECTIVE_SOURCE_BINDING_SCHEMA,
            kind: EFFECTIVE_SOURCE_BINDING_KIND.to_owned(),
            owner: SourceOwnerIdentity {
                canonical_ref: "tool:test/run".to_owned(),
                item_kind: "tool".to_owned(),
                source_space: SourceSpaceIdentity::Project,
                source_root: SourceRootIdentity::Project,
                root_source_content_digest: "a".repeat(64),
                root_raw_content_digest: "b".repeat(64),
                signer_fingerprint: "c".repeat(64),
                logical_item_key: "test/run".to_owned(),
            },
            kind_ceiling: SignedKindSourceCeiling {
                schema_ref: "kind:tool".to_owned(),
                source_content_digest: "d".repeat(64),
                raw_content_digest: schema_body_digest,
                signer_fingerprint: "f".repeat(64),
                signature_header: "signed".to_owned(),
                schema_body,
                schema_document: serde_json::json!({"kind": "kind"}),
                normalized_declaration: serde_json::json!({
                    "derived": SOURCE_CLOSURE_DERIVED_KEY,
                    "location": {"type": "item_namespace"},
                    "testimony": "owner_signed_files",
                    "max_files": 8,
                    "max_total_bytes": 1024,
                    "max_file_bytes": 512,
                    "max_depth": 8,
                }),
                root_kind_format: serde_json::json!({"extensions": ["yaml"]}),
                root_signature_envelope: serde_json::json!({"style": "header"}),
            },
            content_manifest_hash: "1".repeat(64),
            testimony: SourceTestimonyProof::OwnerSignedFiles {
                signer_fingerprint: "c".repeat(64),
                file_count: 2,
                entries_digest: "2".repeat(64),
            },
            execution_policy: SourceExecutionPolicyIdentity::Executor {
                declarer_ref: "tool:ryeos/core/runtimes/python/function".to_owned(),
                signer_fingerprint: "3".repeat(64),
                source_content_digest: "4".repeat(64),
                raw_content_digest: "5".repeat(64),
                policy_digest: "6".repeat(64),
                chain_digest: "7".repeat(64),
            },
            logical_binding: SourceLogicalBinding::Tool {
                loader_roots: vec![SourceLoaderRoot::ItemDirectory],
                root_entry: "run.py".to_owned(),
            },
        };
        binding.validate().unwrap();
        let manifest = SourceClosureManifest::new(
            vec![LogicalSourceRoot {
                id: "source".to_owned(),
            }],
            vec![file("run.py", 'a'), file("lib/z.py", 'b')],
        )
        .unwrap();
        let mut retained = binding.clone();
        retained.content_manifest_hash = manifest.digest().unwrap();
        retained.validate_content_manifest(&manifest).unwrap();
        let SourceTestimonyProof::OwnerSignedFiles { file_count, .. } = &mut retained.testimony
        else {
            unreachable!()
        };
        *file_count = 1;
        assert!(retained.validate_content_manifest(&manifest).is_err());

        let mut wrong_contract = binding.clone();
        wrong_contract.testimony = SourceTestimonyProof::OwnerSignedDigest {
            expected_manifest_hash: wrong_contract.content_manifest_hash.clone(),
        };
        assert!(wrong_contract.validate().is_err());

        let first = binding.digest().unwrap();
        let mut other = binding.clone();
        other.owner.canonical_ref = "tool:test/other".to_owned();
        other.owner.logical_item_key = "test/other".to_owned();
        other.validate().unwrap();
        assert_ne!(first, other.digest().unwrap());
        assert_eq!(binding.content_manifest_hash, other.content_manifest_hash);
    }
}
