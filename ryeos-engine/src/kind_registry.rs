//! Unified kind registry — one validated `KindSchema` per kind.
//!
//! Loaded from `*.kind-schema.yaml` files across the 3-tier space.
//! This is the single source of truth for kind metadata: directory name,
//! default executor, file extensions, parsers, signature envelopes, and
//! daemon resolution pipeline steps.
//!
//! The engine never hardcodes kind names, extension lists, or directory
//! mappings. Adding a new kind = adding a new kind schema YAML.
//!
//! Kind schemas are the bootstrap layer — they define how items are resolved
//! and signed. Therefore kind schema loading uses raw filesystem scanning
//! and a hardcoded signature envelope (`#` prefix). Every kind schema must
//! be signed by a trusted key. Unsigned or tampered schemas are rejected.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::contracts::{ResolvedSourceFormat, SignatureEnvelope};
use crate::error::EngineError;
use crate::trust::TrustStore;

/// A single extension entry within a `KindSchema`.
///
/// Captures the file extension, its metadata parser, and its
/// signature embedding format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionSpec {
    /// File extension including the dot, e.g. `".py"`, `".md"`
    pub ext: String,
    /// Parser ID for lightweight metadata extraction, e.g. `"python/ast"`
    pub parser_id: String,
    /// Signature embedding envelope for this file type
    pub signature: SignatureEnvelope,
}

/// A rule for extracting a single metadata field from a parsed document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractionRule {
    /// Extract from the filename (stem, no extension)
    Filename,
    /// Use a constant value
    Constant { value: String },
    /// Extract from a key path in the parsed document
    Path { key: String },
}

/// Complete schema for a single item kind, loaded from a kind schema
/// YAML. One struct per kind — no parallel maps, no split state.
#[derive(Debug, Clone)]
pub struct KindSchema {
    /// The `.ai/` subdirectory name, e.g. `"tools"`, `"directives"`
    pub directory: String,
    /// Default executor ID when item metadata does not declare one
    pub default_executor_id: Option<String>,
    /// Ordered extension specs — extension priority during resolution
    /// is the order declared in the schema
    pub extensions: Vec<ExtensionSpec>,
    /// Data-driven extraction rules: output field name → rule
    pub extraction_rules: HashMap<String, ExtractionRule>,
    /// Resolver names for the daemon resolution pipeline.
    /// Empty list = no daemon-side resolution (direct execution).
    pub resolution: Vec<String>,
}

impl KindSchema {
    /// Get just the extension strings.
    pub fn extension_strs(&self) -> Vec<&str> {
        self.extensions.iter().map(|s| s.ext.as_str()).collect()
    }

    /// Look up the `ExtensionSpec` for a specific extension.
    pub fn spec_for(&self, ext: &str) -> Option<&ExtensionSpec> {
        self.extensions.iter().find(|s| s.ext == ext)
    }

    /// Build a `ResolvedSourceFormat` from a matched extension.
    pub fn resolved_format_for(&self, ext: &str) -> Option<ResolvedSourceFormat> {
        self.spec_for(ext).map(|spec| ResolvedSourceFormat {
            extension: spec.ext.clone(),
            parser_id: spec.parser_id.clone(),
            signature: spec.signature.clone(),
        })
    }
}

/// Unified kind registry — maps kind strings to `KindSchema`.
///
/// Built in two stages:
///   1. Base registry from user + system space at engine startup
///   2. Project overlay per request after `ProjectContext` materialization
///
/// The loader uses raw filesystem paths — it must NOT depend on
/// normal item resolution to avoid a bootstrap cycle.
#[derive(Debug, Clone)]
pub struct KindRegistry {
    schemas: HashMap<String, KindSchema>,
    fingerprint: String,
}

impl KindRegistry {
    /// Build an empty registry (for testing or before loading).
    pub fn empty() -> Self {
        Self {
            schemas: HashMap::new(),
            fingerprint: "empty".to_owned(),
        }
    }

    /// Load the base registry from user + system kind schema search paths.
    ///
    /// Scans `{kind}/*.kind-schema.yaml` files within each search root.
    /// Uses raw filesystem scanning — no item resolution dependency.
    /// Every kind schema must be signed and verified against the trust store.
    /// Unsigned or tampered schemas cause the entire load to fail.
    pub fn load_base(
        search_roots: &[PathBuf],
        trust_store: &TrustStore,
    ) -> Result<Self, EngineError> {
        let mut schemas: HashMap<String, KindSchema> = HashMap::new();
        let mut fingerprint_data = Vec::new();

        for root in search_roots {
            if !root.exists() {
                continue;
            }
            load_schemas_from_dir(root, &mut schemas, &mut fingerprint_data, false, trust_store)?;
        }

        let fingerprint = lillux::cas::sha256_hex(&fingerprint_data);

        Ok(Self {
            schemas,
            fingerprint,
        })
    }

    /// Apply a project overlay on top of the base registry.
    ///
    /// If a project-space schema defines a kind, it replaces that kind's
    /// entire schema entry — including directory, executor, extensions,
    /// and the `resolution` list. This makes overlay semantics simple
    /// and deterministic.
    ///
    /// Project kind schemas must also be signed and verified.
    /// Returns a new registry with the overlay applied.
    pub fn with_project_overlay(
        &self,
        project_kinds_root: &Path,
        trust_store: &TrustStore,
    ) -> Result<Self, EngineError> {
        if !project_kinds_root.exists() {
            return Ok(self.clone());
        }

        let mut schemas = self.schemas.clone();
        let mut fingerprint_data = self.fingerprint.as_bytes().to_vec();

        load_schemas_from_dir(
            project_kinds_root,
            &mut schemas,
            &mut fingerprint_data,
            true,
            trust_store,
        )?;

        tracing::debug!(path = %project_kinds_root.display(), "applied project kind overlay");

        let fingerprint = lillux::cas::sha256_hex(&fingerprint_data);

        Ok(Self {
            schemas,
            fingerprint,
        })
    }

    /// Get the full schema for a kind.
    pub fn get(&self, kind: &str) -> Option<&KindSchema> {
        self.schemas.get(kind)
    }

    /// Check whether a kind is registered.
    pub fn contains(&self, kind: &str) -> bool {
        self.schemas.contains_key(kind)
    }

    /// Get the directory name for a kind.
    pub fn directory(&self, kind: &str) -> Option<&str> {
        self.schemas.get(kind).map(|s| s.directory.as_str())
    }

    /// Get the default executor ID for a kind.
    pub fn default_executor_id(&self, kind: &str) -> Option<&str> {
        self.schemas
            .get(kind)
            .and_then(|s| s.default_executor_id.as_deref())
    }

    /// Get the ordered extension specs for a kind.
    pub fn extensions(&self, kind: &str) -> Option<&[ExtensionSpec]> {
        self.schemas.get(kind).map(|s| s.extensions.as_slice())
    }

    /// Get just the extension strings for a kind.
    pub fn extension_strs(&self, kind: &str) -> Option<Vec<&str>> {
        self.schemas.get(kind).map(|s| s.extension_strs())
    }

    /// Look up the `ExtensionSpec` for a specific kind + extension pair.
    pub fn spec_for(&self, kind: &str, ext: &str) -> Option<&ExtensionSpec> {
        self.schemas.get(kind)?.spec_for(ext)
    }

    /// Build a `ResolvedSourceFormat` from a matched kind + extension.
    pub fn resolved_format_for(&self, kind: &str, ext: &str) -> Option<ResolvedSourceFormat> {
        self.schemas.get(kind)?.resolved_format_for(ext)
    }

    /// Cache-key fingerprint. Changes when kind schema config changes.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// All registered kind names.
    pub fn kinds(&self) -> impl Iterator<Item = &str> {
        self.schemas.keys().map(|s| s.as_str())
    }

    /// Number of registered kinds.
    pub fn len(&self) -> usize {
        self.schemas.len()
    }

    pub fn is_empty(&self) -> bool {
        self.schemas.is_empty()
    }
}

impl Default for KindRegistry {
    fn default() -> Self {
        Self::empty()
    }
}

// ── Loader implementation ────────────────────────────────────────────

const KIND_SCHEMA_SUFFIX: &str = ".kind-schema.yaml";

fn load_schemas_from_dir(
    kinds_root: &Path,
    schemas: &mut HashMap<String, KindSchema>,
    fingerprint_data: &mut Vec<u8>,
    replace_existing: bool,
    trust_store: &TrustStore,
) -> Result<(), EngineError> {
    let dir_entries = match std::fs::read_dir(kinds_root) {
        Ok(d) => d,
        Err(e) => {
            return Err(EngineError::SchemaLoaderError {
                reason: format!("cannot read kinds dir {}: {e}", kinds_root.display()),
            });
        }
    };

    // Collect and sort kind subdirectories for deterministic ordering
    let mut kind_dirs: Vec<_> = dir_entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    kind_dirs.sort();

    for kind_dir in kind_dirs {
        let kind_name = match kind_dir.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_owned(),
            None => continue,
        };

        // Collect and sort schema files for deterministic ordering
        let yaml_entries = match std::fs::read_dir(&kind_dir) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let mut schema_files: Vec<_> = yaml_entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                name.ends_with(KIND_SCHEMA_SUFFIX) && !name.starts_with('_')
            })
            .collect();
        schema_files.sort();

        for yaml_path in schema_files {
            let parsed = load_and_verify_kind_schema(&yaml_path, trust_store)?;

            if replace_existing {
                // Project overlay replaces base schemas entirely
                schemas.insert(kind_name.clone(), parsed);
                // Always include in fingerprint for overlay
                if let Ok(content) = std::fs::read(&yaml_path) {
                    fingerprint_data.extend_from_slice(&content);
                }
                tracing::debug!(kind = %kind_name, path = %yaml_path.display(), "loaded kind schema");
            } else {
                // First-found wins across roots (matches trust store semantics)
                use std::collections::hash_map::Entry;
                match schemas.entry(kind_name.clone()) {
                    Entry::Vacant(e) => {
                        e.insert(parsed);
                        // Only fingerprint winning schemas
                        if let Ok(content) = std::fs::read(&yaml_path) {
                            fingerprint_data.extend_from_slice(&content);
                        }
                        tracing::debug!(kind = %kind_name, path = %yaml_path.display(), "loaded kind schema");
                    }
                    Entry::Occupied(_) => {
                        tracing::debug!(kind = %kind_name, path = %yaml_path.display(), "skipped shadowed kind schema");
                    }
                }
            }
        }
    }

    Ok(())
}

/// Verify the signature on a kind schema file, then parse it.
///
/// Uses a hardcoded envelope format (`#` prefix, no suffix) because kind
/// schemas are the bootstrap layer — they can't look up their own envelope
/// from a kind schema without circularity. Kind schemas are always YAML.
///
/// Fails closed: unsigned schemas are rejected. One bad schema poisons
/// the entire registry load.
fn load_and_verify_kind_schema(
    yaml_path: &Path,
    trust_store: &TrustStore,
) -> Result<KindSchema, EngineError> {
    let content = std::fs::read_to_string(yaml_path).map_err(|e| {
        EngineError::SchemaLoaderError {
            reason: format!("cannot read {}: {e}", yaml_path.display()),
        }
    })?;

    let prefix = "#";
    let suffix: Option<&str> = None;

    let sig_header = lillux::signature::parse_signature_line(
        content.lines().next().unwrap_or(""),
        prefix,
        suffix,
    );

    match sig_header {
        Some(header) => {
            let body = lillux::signature::strip_signature_lines(&content);
            let actual_hash = lillux::signature::content_hash(&body);

            if actual_hash != header.content_hash {
                return Err(EngineError::ContentHashMismatch {
                    canonical_ref: format!("config:{}", infer_config_id(yaml_path)),
                    expected: header.content_hash,
                    actual: actual_hash,
                });
            }

            let signer = trust_store.get(&header.signer_fingerprint).ok_or_else(|| {
                EngineError::UntrustedSigner {
                    canonical_ref: format!("config:{}", infer_config_id(yaml_path)),
                    fingerprint: header.signer_fingerprint.clone(),
                }
            })?;

            if !lillux::signature::verify_signature(
                &header.content_hash,
                &header.signature_b64,
                &signer.verifying_key,
            ) {
                return Err(EngineError::SignatureVerificationFailed {
                    canonical_ref: format!("config:{}", infer_config_id(yaml_path)),
                    reason: "Ed25519 signature verification failed".into(),
                });
            }
        }
        None => {
            return Err(EngineError::SignatureMissing {
                canonical_ref: format!("config:{}", infer_config_id(yaml_path)),
            });
        }
    }

    parse_kind_schema_content(&yaml_path.display().to_string(), &content)
}

/// Reverse-map a filesystem path to a config item ID.
///
/// Strips the `.ai/config/` prefix and `.kind-schema.yaml` suffix.
/// Example: `.ai/config/engine/kinds/tool/tool.kind-schema.yaml` →
/// `engine/kinds/tool/tool`
fn infer_config_id(yaml_path: &Path) -> String {
    let path_str = yaml_path.to_string_lossy();
    let needle = ".ai/config/";
    if let Some(idx) = path_str.find(needle) {
        let after = &path_str[idx + needle.len()..];
        if let Some(stripped) = after.strip_suffix(".kind-schema.yaml") {
            return stripped.to_string();
        }
    }
    // Path doesn't contain .ai/config/ — use kind_dir/filename_stem
    let kind_dir = yaml_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    let stem = yaml_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .strip_suffix(".kind-schema")
        .unwrap_or("unknown");
    format!("engine/kinds/{kind_dir}/{stem}")
}

/// Parse a kind schema from already-verified content.
///
/// Called by `load_and_verify_kind_schema` after signature verification
/// succeeds. Receives the raw file content (still with signature line —
/// `strip_signature_lines` is called internally).
fn parse_kind_schema_content(display: &str, content: &str) -> Result<KindSchema, EngineError> {
    let clean_content = lillux::signature::strip_signature_lines(content);

    let data: serde_yaml::Value =
        serde_yaml::from_str(&clean_content).map_err(|e| EngineError::SchemaLoaderError {
            reason: format!("cannot parse YAML {display}: {e}"),
        })?;

    let directory = data
        .get("location")
        .and_then(|v| v.get("directory"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .ok_or_else(|| EngineError::SchemaLoaderError {
            reason: format!("{display}: missing required field `location.directory`"),
        })?;

    let default_executor_id = data
        .get("execution")
        .and_then(|v| v.get("default_executor_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());

    let resolution: Vec<String> = data
        .get("resolution")
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_owned()))
                .collect()
        })
        .unwrap_or_default();

    let formats_seq = data
        .get("formats")
        .and_then(|v| v.as_sequence())
        .ok_or_else(|| EngineError::SchemaLoaderError {
            reason: format!("{display}: missing required field `formats`"),
        })?;

    if formats_seq.is_empty() {
        return Err(EngineError::SchemaLoaderError {
            reason: format!("{display}: `formats` list is empty"),
        });
    }

    let mut extensions = Vec::new();
    for (i, entry) in formats_seq.iter().enumerate() {
        let entry_label = format!("formats[{i}]");

        let ext_seq = entry
            .get("extensions")
            .and_then(|v| v.as_sequence())
            .ok_or_else(|| EngineError::SchemaLoaderError {
                reason: format!("{display}: {entry_label} missing `extensions`"),
            })?;

        let ext_strs: Vec<String> = ext_seq
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_owned()))
            .collect();

        if ext_strs.is_empty() {
            return Err(EngineError::SchemaLoaderError {
                reason: format!("{display}: {entry_label} `extensions` list is empty"),
            });
        }

        let parser_id = entry
            .get("parser_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned())
            .ok_or_else(|| EngineError::SchemaLoaderError {
                reason: format!("{display}: {entry_label} missing `parser_id`"),
            })?;

        let sig_value = entry
            .get("signature")
            .ok_or_else(|| EngineError::SchemaLoaderError {
                reason: format!("{display}: {entry_label} missing `signature`"),
            })?;
        let signature = parse_signature_format_strict(sig_value, display)?;

        for ext in ext_strs {
            extensions.push(ExtensionSpec {
                ext,
                parser_id: parser_id.clone(),
                signature: signature.clone(),
            });
        }
    }

    let extraction_rules = parse_extraction_rules(&data, display)?;

    Ok(KindSchema {
        directory,
        default_executor_id,
        extensions,
        extraction_rules,
        resolution,
    })
}

fn parse_signature_format_strict(
    value: &serde_yaml::Value,
    schema_path: &str,
) -> Result<SignatureEnvelope, EngineError> {
    let map = value
        .as_mapping()
        .ok_or_else(|| EngineError::SchemaLoaderError {
            reason: format!("{schema_path}: `signature` must be a mapping"),
        })?;

    let prefix = map
        .iter()
        .find_map(|(k, v)| {
            if k.as_str() == Some("prefix") {
                v.as_str().map(|s| s.to_owned())
            } else {
                None
            }
        })
        .ok_or_else(|| EngineError::SchemaLoaderError {
            reason: format!("{schema_path}: `signature.prefix` is required"),
        })?;

    let suffix = map.iter().find_map(|(k, v)| {
        if k.as_str() == Some("suffix") {
            v.as_str().map(|s| s.to_owned())
        } else {
            None
        }
    });

    let after_shebang = map
        .iter()
        .find_map(|(k, v)| {
            if k.as_str() == Some("after_shebang") {
                v.as_bool()
            } else {
                None
            }
        })
        .unwrap_or(false);

    Ok(SignatureEnvelope {
        prefix,
        suffix,
        after_shebang,
    })
}

fn parse_extraction_rules(
    data: &serde_yaml::Value,
    display: &str,
) -> Result<HashMap<String, ExtractionRule>, EngineError> {
    let mapping = match data
        .get("metadata")
        .and_then(|v| v.get("rules"))
        .and_then(|v| v.as_mapping())
    {
        Some(m) => m,
        None => return Ok(HashMap::new()),
    };

    let mut rules = HashMap::new();
    for (k, v) in mapping {
        let field = k
            .as_str()
            .ok_or_else(|| EngineError::SchemaLoaderError {
                reason: format!("{display}: non-string key in `metadata.rules`"),
            })?
            .to_owned();

        let rule_map = v
            .as_mapping()
            .ok_or_else(|| EngineError::SchemaLoaderError {
                reason: format!("{display}: metadata.rules.{field} must be a mapping"),
            })?;

        let rule_type = rule_map
            .iter()
            .find_map(|(rk, rv)| {
                if rk.as_str() == Some("from") {
                    rv.as_str().map(|s| s.to_owned())
                } else {
                    None
                }
            })
            .ok_or_else(|| EngineError::SchemaLoaderError {
                reason: format!("{display}: metadata.rules.{field} missing `from`"),
            })?;

        let rule = match rule_type.as_str() {
            "filename" => ExtractionRule::Filename,
            "constant" => {
                let value = rule_map
                    .iter()
                    .find_map(|(rk, rv)| {
                        if rk.as_str() == Some("value") {
                            rv.as_str().map(|s| s.to_owned())
                        } else {
                            None
                        }
                    })
                    .ok_or_else(|| EngineError::SchemaLoaderError {
                        reason: format!(
                            "{display}: metadata.rules.{field} from=constant requires `value`"
                        ),
                    })?;
                ExtractionRule::Constant { value }
            }
            "path" => {
                let key = rule_map
                    .iter()
                    .find_map(|(rk, rv)| {
                        if rk.as_str() == Some("key") {
                            rv.as_str().map(|s| s.to_owned())
                        } else {
                            None
                        }
                    })
                    .ok_or_else(|| EngineError::SchemaLoaderError {
                        reason: format!(
                            "{display}: metadata.rules.{field} from=path requires `key`"
                        ),
                    })?;
                ExtractionRule::Path { key }
            }
            other => {
                return Err(EngineError::SchemaLoaderError {
                    reason: format!("{display}: metadata.rules.{field} unknown from `{other}`"),
                });
            }
        };

        rules.insert(field, rule);
    }

    Ok(rules)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::{TrustedSigner, TrustStore};
    use base64::Engine;
    use lillux::crypto::SigningKey;
    use std::fs;

    const TOOL_SCHEMA: &str = "\
location:
  directory: tools
formats:
  - extensions: [\".py\"]
    parser_id: python/ast
    signature:
      prefix: \"#\"
      after_shebang: true
  - extensions: [\".yaml\", \".yml\"]
    parser_id: yaml/yaml
    signature:
      prefix: \"#\"
  - extensions: [\".js\", \".ts\"]
    parser_id: javascript/javascript
    signature:
      prefix: \"//\"
  - extensions: [\".sh\"]
    parser_id: python/ast
    signature:
      prefix: \"#\"
      after_shebang: true
metadata:
  rules:
    name:
      from: filename
    version:
      from: path
      key: __version__
    executor_id:
      from: path
      key: __executor_id__
";

    const DIRECTIVE_SCHEMA: &str = "\
location:
  directory: directives
execution:
  default_executor_id: \"native:directive_orchestrator\"
formats:
  - extensions: [\".md\"]
    parser_id: markdown/xml
    signature:
      prefix: \"<!--\"
      suffix: \"-->\"
metadata:
  rules:
    executor_id:
      from: constant
      value: \"native:directive_orchestrator\"
    version:
      from: path
      key: version
";

    const SCHEMA_WITH_RESOLUTION: &str = "\
location:
  directory: directives
resolution:
  - resolve_extends_chain
  - resolve_provider
  - preload_tool_schemas
formats:
  - extensions: [\".md\"]
    parser_id: markdown/xml
    signature:
      prefix: \"<!--\"
      suffix: \"-->\"
";

    fn test_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[42u8; 32])
    }

    fn test_trust_store(sk: &SigningKey) -> TrustStore {
        let vk = sk.verifying_key();
        let fp = crate::trust::compute_fingerprint(&vk);
        TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp,
            verifying_key: vk,
            label: None,
        }])
    }

    fn sign_and_write_schema(dir: &Path, kind_name: &str, yaml: &str, sk: &SigningKey) {
        let kind_dir = dir.join(kind_name);
        fs::create_dir_all(&kind_dir).unwrap();
        let signed = lillux::signature::sign_content(yaml, sk, "#", None);
        fs::write(
            kind_dir.join(format!("{kind_name}.kind-schema.yaml")),
            signed,
        )
        .unwrap();
    }

    fn write_tool_schema(dir: &Path, sk: &SigningKey) {
        sign_and_write_schema(dir, "tool", TOOL_SCHEMA, sk);
    }

    fn write_directive_schema(dir: &Path, sk: &SigningKey) {
        sign_and_write_schema(dir, "directive", DIRECTIVE_SCHEMA, sk);
    }

    #[test]
    fn load_from_temp_dir() {
        let tmp = tempdir();
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);
        write_tool_schema(&tmp, &sk);
        write_directive_schema(&tmp, &sk);

        let reg = KindRegistry::load_base(&[tmp.clone()], &ts).unwrap();

        // Tool schema
        let tool = reg.get("tool").unwrap();
        assert_eq!(tool.directory, "tools");
        assert!(tool.default_executor_id.is_none());
        let tool_exts = tool.extension_strs();
        assert!(tool_exts.contains(&".py"));
        assert!(tool_exts.contains(&".ts"));
        assert!(tool_exts.contains(&".sh"));

        // Directive schema
        let dir = reg.get("directive").unwrap();
        assert_eq!(dir.directory, "directives");
        assert_eq!(
            dir.default_executor_id.as_deref(),
            Some("native:directive_orchestrator")
        );
        assert_eq!(dir.extension_strs(), vec![".md"]);

        // Parser lookups
        let py_spec = reg.spec_for("tool", ".py").unwrap();
        assert_eq!(py_spec.parser_id, "python/ast");

        let ts_spec = reg.spec_for("tool", ".ts").unwrap();
        assert_eq!(ts_spec.parser_id, "javascript/javascript");
        assert_eq!(ts_spec.signature.prefix, "//");
        assert!(!ts_spec.signature.after_shebang);

        let md_spec = reg.spec_for("directive", ".md").unwrap();
        assert_eq!(md_spec.parser_id, "markdown/xml");
        assert_eq!(md_spec.signature.prefix, "<!--");
        assert_eq!(md_spec.signature.suffix.as_deref(), Some("-->"));

        // Fingerprint
        assert!(!reg.fingerprint().is_empty());
        assert_ne!(reg.fingerprint(), "empty");
    }

    #[test]
    fn convenience_accessors() {
        let tmp = tempdir();
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);
        write_tool_schema(&tmp, &sk);
        write_directive_schema(&tmp, &sk);

        let reg = KindRegistry::load_base(&[tmp], &ts).unwrap();

        assert_eq!(reg.directory("tool"), Some("tools"));
        assert_eq!(reg.directory("directive"), Some("directives"));
        assert_eq!(
            reg.default_executor_id("directive"),
            Some("native:directive_orchestrator")
        );
        assert_eq!(reg.default_executor_id("tool"), None);

        assert!(reg.contains("tool"));
        assert!(!reg.contains("nonexistent"));

        assert_eq!(reg.len(), 2);
        assert!(!reg.is_empty());
    }

    #[test]
    fn project_overlay_replaces_kind() {
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);

        let system = tempdir();
        write_tool_schema(&system, &sk);

        let base = KindRegistry::load_base(&[system], &ts).unwrap();
        assert_eq!(base.extension_strs("tool").unwrap().len(), 6);

        // Project schema replaces the entire tool kind with just .rb
        let project = tempdir();
        let rb_yaml = "\
location:
  directory: tools
formats:
  - extensions: [\".rb\"]
    parser_id: ruby/ruby
    signature:
      prefix: \"#\"
      after_shebang: true
";
        sign_and_write_schema(&project, "tool", rb_yaml, &sk);

        let overlaid = base.with_project_overlay(&project, &ts).unwrap();

        // Project overlay fully replaced the kind — only .rb now
        let tool_exts = overlaid.extension_strs("tool").unwrap();
        assert_eq!(tool_exts, vec![".rb"]);
        assert!(!tool_exts.contains(&".py"));

        // Fingerprint changed
        assert_ne!(base.fingerprint(), overlaid.fingerprint());
    }

    #[test]
    fn resolved_format_for() {
        let tmp = tempdir();
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);
        write_tool_schema(&tmp, &sk);

        let reg = KindRegistry::load_base(&[tmp], &ts).unwrap();
        let fmt = reg.resolved_format_for("tool", ".py").unwrap();
        assert_eq!(fmt.extension, ".py");
        assert_eq!(fmt.parser_id, "python/ast");
        assert_eq!(fmt.signature.prefix, "#");
        assert!(fmt.signature.after_shebang);

        assert!(reg.resolved_format_for("tool", ".xyz").is_none());
        assert!(reg.resolved_format_for("nonexistent", ".py").is_none());
    }

    #[test]
    fn empty_registry() {
        let reg = KindRegistry::empty();
        assert!(reg.get("tool").is_none());
        assert!(reg.is_empty());
        assert_eq!(reg.fingerprint(), "empty");
    }

    #[test]
    fn reject_unsigned_kind_schema() {
        let tmp = tempdir();
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);
        // Write unsigned schema
        let tool_dir = tmp.join("tool");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(
            tool_dir.join("tool.kind-schema.yaml"),
            "location:\n  directory: tools\nformats:\n  - extensions: [\".py\"]\n    parser_id: python/ast\n    signature:\n      prefix: \"#\"\n",
        )
        .unwrap();

        let err = KindRegistry::load_base(&[tmp], &ts).unwrap_err();
        assert!(
            matches!(err, EngineError::SignatureMissing { .. }),
            "expected SignatureMissing, got: {err:?}"
        );
    }

    #[test]
    fn reject_tampered_kind_schema() {
        let tmp = tempdir();
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);
        write_tool_schema(&tmp, &sk);

        // Tamper: append a line to the signed file
        let schema_path = tmp.join("tool").join("tool.kind-schema.yaml");
        let mut content = fs::read_to_string(&schema_path).unwrap();
        content.push_str("# injected\n");
        fs::write(&schema_path, content).unwrap();

        let err = KindRegistry::load_base(&[tmp], &ts).unwrap_err();
        assert!(
            matches!(err, EngineError::ContentHashMismatch { .. }),
            "expected ContentHashMismatch, got: {err:?}"
        );
    }

    #[test]
    fn reject_untrusted_key() {
        let tmp = tempdir();
        let sk = test_signing_key();
        // Trust store with a DIFFERENT key
        let bad_sk = SigningKey::from_bytes(&[99u8; 32]);
        let ts = test_trust_store(&bad_sk);
        write_tool_schema(&tmp, &sk);

        let err = KindRegistry::load_base(&[tmp], &ts).unwrap_err();
        assert!(
            matches!(err, EngineError::UntrustedSigner { .. }),
            "expected UntrustedSigner, got: {err:?}"
        );
    }

    #[test]
    fn reject_bad_signature() {
        let tmp = tempdir();
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);
        write_tool_schema(&tmp, &sk);

        let schema_path = tmp.join("tool").join("tool.kind-schema.yaml");
        let content = fs::read_to_string(&schema_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        let sig_line = lines[0];

        // Reconstruct the sig line with a garbage base64 signature
        let parts: Vec<&str> = sig_line.rsplitn(4, ':').collect();
        let fp = parts[0];
        let hash = parts[2];
        let prefix_and_ts = parts[3];
        let bad_sig = base64::engine::general_purpose::STANDARD.encode([0u8; 64]);
        let bad_line = format!(
            "{} rye:signed:{}:{}:{}:{}",
            "#",
            prefix_and_ts,
            hash,
            bad_sig,
            fp
        );
        let mut new_content = bad_line;
        for line in &lines[1..] {
            new_content.push('\n');
            new_content.push_str(line);
        }
        new_content.push('\n');
        fs::write(&schema_path, new_content).unwrap();

        let err = KindRegistry::load_base(&[tmp], &ts).unwrap_err();
        assert!(
            matches!(err, EngineError::SignatureVerificationFailed { .. }),
            "expected SignatureVerificationFailed, got: {err:?}"
        );
    }

    #[test]
    fn reject_missing_location_directory() {
        let tmp = tempdir();
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);
        let yaml = "\
formats:
  - extensions: [\".py\"]
    parser_id: python/ast
    signature:
      prefix: \"#\"
      after_shebang: true
";
        sign_and_write_schema(&tmp, "tool", yaml, &sk);

        let err = KindRegistry::load_base(&[tmp], &ts).unwrap_err();
        assert!(
            matches!(err, EngineError::SchemaLoaderError { ref reason } if reason.contains("location.directory")),
            "expected location.directory error, got: {err:?}"
        );
    }

    #[test]
    fn reject_missing_formats() {
        let tmp = tempdir();
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);
        let yaml = "\
location:
  directory: tools
";
        sign_and_write_schema(&tmp, "tool", yaml, &sk);

        let err = KindRegistry::load_base(&[tmp], &ts).unwrap_err();
        assert!(
            matches!(err, EngineError::SchemaLoaderError { ref reason } if reason.contains("formats")),
            "expected formats error, got: {err:?}"
        );
    }

    #[test]
    fn reject_missing_parser_id() {
        let tmp = tempdir();
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);
        let yaml = "\
location:
  directory: tools
formats:
  - extensions: [\".py\"]
    signature:
      prefix: \"#\"
      after_shebang: true
";
        sign_and_write_schema(&tmp, "tool", yaml, &sk);

        let err = KindRegistry::load_base(&[tmp], &ts).unwrap_err();
        assert!(
            matches!(err, EngineError::SchemaLoaderError { ref reason } if reason.contains("parser_id")),
            "expected parser_id error, got: {err:?}"
        );
    }

    #[test]
    fn reject_missing_signature() {
        let tmp = tempdir();
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);
        let yaml = "\
location:
  directory: tools
formats:
  - extensions: [\".py\"]
    parser_id: python/ast
";
        sign_and_write_schema(&tmp, "tool", yaml, &sk);

        let err = KindRegistry::load_base(&[tmp], &ts).unwrap_err();
        assert!(
            matches!(err, EngineError::SchemaLoaderError { ref reason } if reason.contains("signature")),
            "expected signature error, got: {err:?}"
        );
    }

    #[test]
    fn extraction_rules_loaded_from_schema() {
        let tmp = tempdir();
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);
        write_tool_schema(&tmp, &sk);
        write_directive_schema(&tmp, &sk);

        let reg = KindRegistry::load_base(&[tmp], &ts).unwrap();

        // Tool schema has filename, path×2 rules
        let tool = reg.get("tool").unwrap();
        assert_eq!(tool.extraction_rules.len(), 3);
        assert_eq!(
            tool.extraction_rules.get("name"),
            Some(&ExtractionRule::Filename)
        );
        assert_eq!(
            tool.extraction_rules.get("version"),
            Some(&ExtractionRule::Path {
                key: "__version__".into()
            })
        );
        assert_eq!(
            tool.extraction_rules.get("executor_id"),
            Some(&ExtractionRule::Path {
                key: "__executor_id__".into()
            })
        );

        // Directive schema has constant + path rules
        let dir = reg.get("directive").unwrap();
        assert_eq!(dir.extraction_rules.len(), 2);
        assert_eq!(
            dir.extraction_rules.get("executor_id"),
            Some(&ExtractionRule::Constant {
                value: "native:directive_orchestrator".into()
            })
        );
        assert_eq!(
            dir.extraction_rules.get("version"),
            Some(&ExtractionRule::Path {
                key: "version".into()
            })
        );
    }

    #[test]
    fn extraction_rules_optional() {
        let tmp = tempdir();
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);
        let yaml = "\
location:
  directory: tools
formats:
  - extensions: [\".py\"]
    parser_id: python/ast
    signature:
      prefix: \"#\"
      after_shebang: true
";
        sign_and_write_schema(&tmp, "tool", yaml, &sk);

        let reg = KindRegistry::load_base(&[tmp], &ts).unwrap();
        let tool = reg.get("tool").unwrap();
        assert!(tool.extraction_rules.is_empty());
    }

    #[test]
    fn resolution_field_parsed() {
        let tmp = tempdir();
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);
        sign_and_write_schema(&tmp, "directive", SCHEMA_WITH_RESOLUTION, &sk);

        let reg = KindRegistry::load_base(&[tmp], &ts).unwrap();
        let dir = reg.get("directive").unwrap();
        assert_eq!(
            dir.resolution,
            vec![
                "resolve_extends_chain",
                "resolve_provider",
                "preload_tool_schemas",
            ]
        );
    }

    #[test]
    fn resolution_defaults_to_empty() {
        let tmp = tempdir();
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);
        write_tool_schema(&tmp, &sk);

        let reg = KindRegistry::load_base(&[tmp], &ts).unwrap();
        let tool = reg.get("tool").unwrap();
        assert!(tool.resolution.is_empty());
    }

    #[test]
    fn project_overlay_replaces_resolution() {
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);

        let system = tempdir();
        sign_and_write_schema(&system, "directive", SCHEMA_WITH_RESOLUTION, &sk);

        let base = KindRegistry::load_base(&[system], &ts).unwrap();
        let dir = base.get("directive").unwrap();
        assert_eq!(dir.resolution.len(), 3);

        // Project overlay replaces with empty resolution
        let project = tempdir();
        let no_res = "\
location:
  directory: directives
formats:
  - extensions: [\".md\"]
    parser_id: markdown/xml
    signature:
      prefix: \"<!--\"
      suffix: \"-->\"
";
        sign_and_write_schema(&project, "directive", no_res, &sk);

        let overlaid = base.with_project_overlay(&project, &ts).unwrap();
        let dir = overlaid.get("directive").unwrap();
        assert!(dir.resolution.is_empty());
    }

    #[test]
    fn infer_config_id_extracts_path() {
        let path = Path::new("/home/user/.ai/config/engine/kinds/tool/tool.kind-schema.yaml");
        assert_eq!(infer_config_id(path), "engine/kinds/tool/tool");
    }

    #[test]
    fn infer_config_id_no_ai_config_prefix() {
        let path = Path::new("/tmp/test_123/tool/tool.kind-schema.yaml");
        assert_eq!(infer_config_id(path), "engine/kinds/tool/tool");
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rye_engine_test_{}",
            std::process::id() as u64 * 1000 + rand_u64()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rand_u64() -> u64 {
        use std::time::SystemTime;
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64
    }
}
