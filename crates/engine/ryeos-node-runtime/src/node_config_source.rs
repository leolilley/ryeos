//! Shared signed node-source intake and bundle registration data.
//! Domain section handlers and service admission remain in App.

use anyhow::{Context, Result, bail};
use ryeos_engine::contracts::SignatureEnvelope;
use ryeos_engine::trust::TrustStore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Loader-derived structural context for a node-config item.
#[derive(Debug, Clone)]
pub struct NodeItemContext {
    /// Section name selected by `.ai/node/<section>/...`.
    pub section: String,
    /// Relative item id below the section root, without extension.
    pub id: String,
    /// Filename stem.
    pub stem: String,
    /// Path relative to the section root, including extension.
    pub rel_path: PathBuf,
    /// Absolute source file path.
    pub source_file: PathBuf,
    /// Trusted signer fingerprint from the verified signature.
    pub signer_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleRecord {
    /// Bundle name (filename without extension).
    pub name: String,
    /// Absolute, canonicalized path to the bundle root directory.
    pub path: PathBuf,
    /// Path to the `.yaml` file that declared this record.
    pub source_file: PathBuf,
}

/// Signature/trust-checked source supplied only by the shared pinned reader.
/// Fields stay private so callers cannot construct or alter its verified facts.
pub struct VerifiedNodeConfigItem {
    path: PathBuf,
    ctx: NodeItemContext,
    signer_fingerprint: String,
    signer_verifying_key: [u8; 32],
    body: Value,
    signed_source: std::sync::Arc<str>,
}

impl VerifiedNodeConfigItem {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn context(&self) -> &NodeItemContext {
        &self.ctx
    }
    pub fn signer_fingerprint(&self) -> &str {
        &self.signer_fingerprint
    }
    pub fn signer_verifying_key(&self) -> [u8; 32] {
        self.signer_verifying_key
    }
    pub fn body(&self) -> &Value {
        &self.body
    }
    pub fn signed_source(&self) -> &std::sync::Arc<str> {
        &self.signed_source
    }
}

/// Signature envelope used for all node-config items.
fn node_config_envelope() -> SignatureEnvelope {
    SignatureEnvelope {
        prefix: "#".into(),
        suffix: None,
        after_shebang: false,
    }
}

const MAX_NODE_CONFIG_ENTRIES_PER_ROOT: usize = 512;
const MAX_NODE_CONFIG_DEPTH: usize = 32;

/// Recursively collect all `.yaml`/`.yml` regular files under a directory.
///
/// Returns files in deterministic depth-first order, with entries sorted
/// alphabetically at each directory level. Rejects symlinks at every level
/// (both files and directories).
pub fn scan_yaml_files_recursive(dir: &Path) -> Result<Vec<lillux::PinnedRegularFile>> {
    scan_yaml_files(dir, true)
}

pub fn scan_yaml_files(dir: &Path, recursive: bool) -> Result<Vec<lillux::PinnedRegularFile>> {
    let files = lillux::collect_pinned_regular_files_no_follow_bounded(
        dir,
        recursive,
        lillux::DirectoryTraversalBudget::new(
            MAX_NODE_CONFIG_ENTRIES_PER_ROOT,
            MAX_NODE_CONFIG_DEPTH,
        ),
    )?
    .unwrap_or_default();
    for file in &files {
        let path = file.path();
        let ext = path.extension().and_then(|extension| extension.to_str());
        if !matches!(ext, Some("yaml" | "yml")) {
            bail!(
                "node config directory contains unsupported non-YAML entry {}",
                path.display()
            );
        }
    }
    Ok(files)
}

/// Verify signature, trust, and section invariant for a single node-config file.
///
/// The `section_root` is the `.ai/node/<section>/` directory (e.g. the
/// `routes/` dir). The invariant checks that:
/// 1. `file` lives under `section_root` (at any depth)
pub fn verify_and_parse(
    file: &lillux::PinnedRegularFile,
    section_root: &Path,
    expected_section: &str,
    trust_store: &TrustStore,
) -> Result<VerifiedNodeConfigItem> {
    let path = file.path();
    let ext = path.extension().and_then(|e| e.to_str());
    if ext != Some("yaml") && ext != Some("yml") {
        bail!(
            "node config item at {} is not a .yaml or .yml file",
            path.display()
        );
    }

    let stem = path.file_stem().and_then(|s| s.to_str()).context(format!(
        "node config item at {} has no filename stem",
        path.display()
    ))?;

    let content = file.read_bounded(crate::node_document::MAX_ITEM_BYTES)?;
    let content = std::str::from_utf8(&content)
        .with_context(|| format!("node config item at {} is not UTF-8", path.display()))?;

    let envelope = node_config_envelope();

    let header = ryeos_engine::item_resolution::parse_signature_header(&content, &envelope)
        .with_context(|| {
            format!(
                "node config item at {} has no valid signature line",
                path.display()
            )
        })?;
    let signer_fingerprint = header.signer_fingerprint.clone();

    let (trust_class, _) =
        ryeos_engine::trust::verify_item_signature(&content, &header, &envelope, trust_store)?;

    if trust_class != ryeos_engine::contracts::TrustClass::Trusted {
        bail!(
            "node config item at {} is not trusted (trust_class: {:?}); \
             only trusted items are allowed in node config",
            path.display(),
            trust_class
        );
    }
    let signer_verifying_key = trust_store
        .get(&signer_fingerprint)
        .context("verified node-config signer disappeared from trust store")?
        .verifying_key
        .to_bytes();

    let body_str = strip_signature(&content);
    let body: Value = serde_yaml::from_str(&body_str)
        .with_context(|| format!("failed to parse YAML body of {}", path.display()))?;

    // Check section invariant: file must live under section_root. Section is
    // selected by path, not duplicated in the body.
    if !path.starts_with(section_root) {
        bail!(
            "node config item at {} is not under expected section directory '{}' \
             (section containment invariant violated)",
            path.display(),
            section_root.display()
        );
    }

    for forbidden in ["category", "section"] {
        if body.get(forbidden).is_some() {
            bail!(
                "node config item at {} declares path-owned structural field '{}' \
                 (section/category are derived from path and must not be in node YAML)",
                path.display(),
                forbidden
            );
        }
    }

    let rel_path = path.strip_prefix(section_root).with_context(|| {
        format!(
            "node config item at {} is not under expected section directory {}",
            path.display(),
            section_root.display()
        )
    })?;
    let mut id_path = rel_path.to_path_buf();
    id_path.set_extension("");
    let id = id_path
        .components()
        .map(|component| match component {
            std::path::Component::Normal(part) => part
                .to_str()
                .map(|s| s.to_string())
                .context("node config path contains non-UTF-8 segment"),
            _ => bail!("node config relative path contains non-normal segment"),
        })
        .collect::<Result<Vec<_>>>()?
        .join("/");

    if id.is_empty() {
        bail!(
            "node config item at {} has empty path-derived id",
            path.display()
        );
    }

    let ctx = NodeItemContext {
        section: expected_section.to_string(),
        id,
        stem: stem.to_string(),
        rel_path: rel_path.to_path_buf(),
        source_file: path.to_path_buf(),
        signer_fingerprint: signer_fingerprint.clone(),
    };

    Ok(VerifiedNodeConfigItem {
        path: path.to_path_buf(),
        ctx,
        signer_fingerprint,
        signer_verifying_key,
        body,
        signed_source: content.into(),
    })
}

pub fn strip_signature(content: &str) -> String {
    content
        .lines()
        .skip_while(|l| l.starts_with("# ryeos:signed:"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_start()
        .to_string()
}

/// Fail-closed collision detection for bundle records.
///
/// Errors if two records share either:
/// - the same canonical path (the engine would mount the same bundle twice), or
/// - the same name (accidental duplicate registration).
///
/// All `BundleRecord::path` values must already be canonicalized before this
/// is called.
pub fn check_bundle_collisions(records: &[BundleRecord]) -> Result<()> {
    let mut by_path: HashMap<&Path, &BundleRecord> = HashMap::new();
    let mut by_name: HashMap<&str, &BundleRecord> = HashMap::new();

    for record in records {
        if let Some(prev) = by_name.get(record.name.as_str()) {
            bail!(
                "node config section 'bundles' has duplicate name '{}': \
                 first registered from '{}' (path: {}), second from '{}' (path: {})",
                record.name,
                prev.source_file.display(),
                prev.path.display(),
                record.source_file.display(),
                record.path.display(),
            );
        }
        if let Some(prev) = by_path.get(record.path.as_path()) {
            bail!(
                "node config section 'bundles' has duplicate canonical path '{}': \
                 first registered as '{}' (from {}), second as '{}' (from {})",
                record.path.display(),
                prev.name,
                prev.source_file.display(),
                record.name,
                record.source_file.display(),
            );
        }
        by_name.insert(record.name.as_str(), record);
        by_path.insert(record.path.as_path(), record);
    }
    Ok(())
}
