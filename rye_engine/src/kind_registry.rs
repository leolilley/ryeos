//! Unified kind registry — one validated `KindSchema` per kind.
//!
//! Loaded from `*.kind-schema.yaml` files across the 3-tier space.
//! This is the single source of truth for kind metadata: directory name,
//! default executor, file extensions, parsers, and signature envelopes.
//!
//! The engine never hardcodes kind names, extension lists, or directory
//! mappings. Adding a new kind = adding a new kind schema YAML.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::contracts::{ResolvedSourceFormat, SignatureEnvelope};
use crate::error::EngineError;

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
    pub fn load_base(search_roots: &[PathBuf]) -> Result<Self, EngineError> {
        let mut schemas: HashMap<String, KindSchema> = HashMap::new();
        let mut fingerprint_data = Vec::new();

        for root in search_roots {
            if !root.exists() {
                continue;
            }
            load_schemas_from_dir(root, &mut schemas, &mut fingerprint_data)?;
        }

        let fingerprint = hex_digest(&fingerprint_data);

        Ok(Self {
            schemas,
            fingerprint,
        })
    }

    /// Apply a project overlay on top of the base registry.
    ///
    /// If a project-space schema defines a kind, it replaces that kind's
    /// entire schema entry — including directory, executor, and extensions.
    /// This makes overlay semantics simple and deterministic.
    ///
    /// Returns a new registry with the overlay applied.
    pub fn with_project_overlay(
        &self,
        project_kinds_root: &Path,
    ) -> Result<Self, EngineError> {
        if !project_kinds_root.exists() {
            return Ok(self.clone());
        }

        let mut schemas = self.schemas.clone();
        let mut fingerprint_data = self.fingerprint.as_bytes().to_vec();

        load_schemas_from_dir(project_kinds_root, &mut schemas, &mut fingerprint_data)?;

        tracing::debug!(path = %project_kinds_root.display(), "applied project kind overlay");

        let fingerprint = hex_digest(&fingerprint_data);

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
) -> Result<(), EngineError> {
    let dir_entries = match std::fs::read_dir(kinds_root) {
        Ok(d) => d,
        Err(e) => {
            return Err(EngineError::SchemaLoaderError {
                reason: format!(
                    "cannot read kinds dir {}: {e}",
                    kinds_root.display()
                ),
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
            let parsed = parse_kind_schema(&yaml_path)?;

            // Include file content in fingerprint (deterministic order)
            if let Ok(content) = std::fs::read(&yaml_path) {
                fingerprint_data.extend_from_slice(&content);
            }

            tracing::debug!(kind = %kind_name, path = %yaml_path.display(), "loaded kind schema");

            // Replace the entire kind schema (last-found wins within a
            // directory layer; project overlay replaces base entirely)
            schemas.insert(kind_name.clone(), parsed);
        }
    }

    Ok(())
}

fn parse_kind_schema(path: &Path) -> Result<KindSchema, EngineError> {
    let display = path.display().to_string();
    let content = std::fs::read_to_string(path).map_err(|e| EngineError::SchemaLoaderError {
        reason: format!("cannot read {display}: {e}"),
    })?;

    // Strip signature line if present
    let clean_content = strip_signature_lines(&content);

    let data: serde_yaml::Value =
        serde_yaml::from_str(&clean_content).map_err(|e| EngineError::SchemaLoaderError {
            reason: format!("cannot parse YAML {display}: {e}"),
        })?;

    // location.directory is required
    let directory = data
        .get("location")
        .and_then(|v| v.get("directory"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .ok_or_else(|| EngineError::SchemaLoaderError {
            reason: format!("{display}: missing required field `location.directory`"),
        })?;

    // execution.default_executor_id is optional
    let default_executor_id = data
        .get("execution")
        .and_then(|v| v.get("default_executor_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());

    // formats is required and must be non-empty
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

        let sig_value = entry.get("signature").ok_or_else(|| EngineError::SchemaLoaderError {
            reason: format!("{display}: {entry_label} missing `signature`"),
        })?;
        let signature = parse_signature_format_strict(sig_value, &display)?;

        for ext in ext_strs {
            extensions.push(ExtensionSpec {
                ext,
                parser_id: parser_id.clone(),
                signature: signature.clone(),
            });
        }
    }

    // metadata.rules is optional
    let extraction_rules = parse_extraction_rules(&data, &display)?;

    Ok(KindSchema {
        directory,
        default_executor_id,
        extensions,
        extraction_rules,
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
                    reason: format!(
                        "{display}: metadata.rules.{field} unknown from `{other}`"
                    ),
                });
            }
        };

        rules.insert(field, rule);
    }

    Ok(rules)
}

fn strip_signature_lines(content: &str) -> String {
    content
        .lines()
        .filter(|line| !line.trim_start().starts_with("# rye:signed:"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn hex_digest(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for byte in hash.iter() {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn write_tool_schema(dir: &Path) {
        let tool_dir = dir.join("tool");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(tool_dir.join("tool.kind-schema.yaml"), TOOL_SCHEMA).unwrap();
    }

    fn write_directive_schema(dir: &Path) {
        let directive_dir = dir.join("directive");
        fs::create_dir_all(&directive_dir).unwrap();
        fs::write(
            directive_dir.join("directive.kind-schema.yaml"),
            DIRECTIVE_SCHEMA,
        )
        .unwrap();
    }

    #[test]
    fn load_from_temp_dir() {
        let tmp = tempdir();
        write_tool_schema(&tmp);
        write_directive_schema(&tmp);

        let reg = KindRegistry::load_base(&[tmp.clone()]).unwrap();

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
        write_tool_schema(&tmp);
        write_directive_schema(&tmp);

        let reg = KindRegistry::load_base(&[tmp]).unwrap();

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
        let system = tempdir();
        write_tool_schema(&system);

        let base = KindRegistry::load_base(&[system]).unwrap();
        assert_eq!(base.extension_strs("tool").unwrap().len(), 6);

        // Project schema replaces the entire tool kind with just .rb
        let project = tempdir();
        let tool_dir = project.join("tool");
        fs::create_dir_all(&tool_dir).unwrap();
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
        fs::write(tool_dir.join("tool.kind-schema.yaml"), rb_yaml).unwrap();

        let overlaid = base.with_project_overlay(&project).unwrap();

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
        write_tool_schema(&tmp);

        let reg = KindRegistry::load_base(&[tmp]).unwrap();
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
    fn reject_missing_location_directory() {
        let tmp = tempdir();
        let tool_dir = tmp.join("tool");
        fs::create_dir_all(&tool_dir).unwrap();
        let yaml = "\
formats:
  - extensions: [\".py\"]
    parser_id: python/ast
    signature:
      prefix: \"#\"
      after_shebang: true
";
        fs::write(tool_dir.join("tool.kind-schema.yaml"), yaml).unwrap();

        let err = KindRegistry::load_base(&[tmp]).unwrap_err();
        assert!(
            matches!(err, EngineError::SchemaLoaderError { ref reason } if reason.contains("location.directory")),
            "expected location.directory error, got: {err:?}"
        );
    }

    #[test]
    fn reject_missing_formats() {
        let tmp = tempdir();
        let tool_dir = tmp.join("tool");
        fs::create_dir_all(&tool_dir).unwrap();
        let yaml = "\
location:
  directory: tools
";
        fs::write(tool_dir.join("tool.kind-schema.yaml"), yaml).unwrap();

        let err = KindRegistry::load_base(&[tmp]).unwrap_err();
        assert!(
            matches!(err, EngineError::SchemaLoaderError { ref reason } if reason.contains("formats")),
            "expected formats error, got: {err:?}"
        );
    }

    #[test]
    fn reject_missing_parser_id() {
        let tmp = tempdir();
        let tool_dir = tmp.join("tool");
        fs::create_dir_all(&tool_dir).unwrap();
        let yaml = "\
location:
  directory: tools
formats:
  - extensions: [\".py\"]
    signature:
      prefix: \"#\"
      after_shebang: true
";
        fs::write(tool_dir.join("tool.kind-schema.yaml"), yaml).unwrap();

        let err = KindRegistry::load_base(&[tmp]).unwrap_err();
        assert!(
            matches!(err, EngineError::SchemaLoaderError { ref reason } if reason.contains("parser_id")),
            "expected parser_id error, got: {err:?}"
        );
    }

    #[test]
    fn reject_missing_signature() {
        let tmp = tempdir();
        let tool_dir = tmp.join("tool");
        fs::create_dir_all(&tool_dir).unwrap();
        let yaml = "\
location:
  directory: tools
formats:
  - extensions: [\".py\"]
    parser_id: python/ast
";
        fs::write(tool_dir.join("tool.kind-schema.yaml"), yaml).unwrap();

        let err = KindRegistry::load_base(&[tmp]).unwrap_err();
        assert!(
            matches!(err, EngineError::SchemaLoaderError { ref reason } if reason.contains("signature")),
            "expected signature error, got: {err:?}"
        );
    }

    #[test]
    fn extraction_rules_loaded_from_schema() {
        let tmp = tempdir();
        write_tool_schema(&tmp);
        write_directive_schema(&tmp);

        let reg = KindRegistry::load_base(&[tmp]).unwrap();

        // Tool schema has filename, path×2 rules
        let tool = reg.get("tool").unwrap();
        assert_eq!(tool.extraction_rules.len(), 3);
        assert_eq!(tool.extraction_rules.get("name"), Some(&ExtractionRule::Filename));
        assert_eq!(
            tool.extraction_rules.get("version"),
            Some(&ExtractionRule::Path { key: "__version__".into() })
        );
        assert_eq!(
            tool.extraction_rules.get("executor_id"),
            Some(&ExtractionRule::Path { key: "__executor_id__".into() })
        );

        // Directive schema has constant + path rules
        let dir = reg.get("directive").unwrap();
        assert_eq!(dir.extraction_rules.len(), 2);
        assert_eq!(
            dir.extraction_rules.get("executor_id"),
            Some(&ExtractionRule::Constant { value: "native:directive_orchestrator".into() })
        );
        assert_eq!(
            dir.extraction_rules.get("version"),
            Some(&ExtractionRule::Path { key: "version".into() })
        );
    }

    #[test]
    fn extraction_rules_optional() {
        let tmp = tempdir();
        let tool_dir = tmp.join("tool");
        fs::create_dir_all(&tool_dir).unwrap();
        // Schema without metadata.rules
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
        fs::write(tool_dir.join("tool.kind-schema.yaml"), yaml).unwrap();

        let reg = KindRegistry::load_base(&[tmp]).unwrap();
        let tool = reg.get("tool").unwrap();
        assert!(tool.extraction_rules.is_empty());
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
