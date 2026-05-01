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

use serde_json::Value;

use crate::contracts::{ItemMetadata, ResolvedSourceFormat, SignatureEnvelope, ValueShape};
use crate::error::EngineError;
use crate::trust::TrustStore;
use crate::resolution::decl::ResolutionStepDecl;

/// Apply extraction rules to a parser-produced `Value`, populating an
/// `ItemMetadata`. Lives in `kind_registry` because the rules ARE part
/// of the kind schema; it's no longer in `metadata.rs` (deleted).
pub fn apply_extraction_rules(
    parsed: &Value,
    rules: &HashMap<String, MetadataRule>,
    file_path: &Path,
) -> ItemMetadata {
    let mut metadata = ItemMetadata::default();

    for (field, rule) in rules {
        let result = extract_rule_value(&rule.extractor, parsed, file_path);
        assign_extracted_field(&mut metadata, field, result);
    }

    metadata
}

/// Extract the raw rule value (used by both the metadata builder and
/// the path-anchoring validator).
fn extract_rule_value(
    extractor: &ExtractionRule,
    parsed: &Value,
    file_path: &Path,
) -> RuleResult {
    match extractor {
        ExtractionRule::Filename => RuleResult::String(
            file_path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_owned()),
        ),
        ExtractionRule::Constant { value } => RuleResult::String(Some(value.clone())),
        ExtractionRule::Path { key } => {
            RuleResult::String(extract_string_from_value(parsed, key))
        }
        ExtractionRule::PathStringSeq { key } => {
            RuleResult::StringSeq(extract_string_seq_from_value(parsed, key))
        }
    }
}

/// Result of a single extraction rule. A rule produces either a
/// scalar string (most metadata fields) or a sequence of strings
/// (e.g. `required_secrets`). Field-name → typed-slot routing is done
/// downstream in `assign_extracted_field`, NOT inside the per-rule
/// arms, so adding a new typed metadata slot doesn't fork the rule
/// dispatcher.
enum RuleResult {
    String(Option<String>),
    StringSeq(Vec<String>),
}

/// Route an extracted value into the typed `ItemMetadata` slot named
/// by `field`, or fall back to `extra` for unknown names. Routing
/// rejects a type mismatch (e.g. a `path_string_seq` rule pointed at
/// `version`) by silently dropping the value rather than corrupting
/// the typed slot — boot validation already proves the contract
/// shape, so a runtime mismatch here is a misconfigured kind YAML
/// and the loader sees the missing typed value.
fn assign_extracted_field(metadata: &mut ItemMetadata, field: &str, result: RuleResult) {
    match (field, result) {
        ("executor_id", RuleResult::String(Some(v))) => metadata.executor_id = Some(v),
        // The parser kind schema's `handler:` field is the canonical
        // ref to a HandlerRegistry entry — semantically distinct from
        // the tool/runtime executor-chain `executor_id` slot used by
        // inventory/plan_builder/dispatch. Route it generically into
        // `extra["handler"]`; the parser descriptor loader does its
        // own typed deserialization of the field at parse time.
        ("version", RuleResult::String(Some(v))) => metadata.version = Some(v),
        ("description", RuleResult::String(Some(v))) => metadata.description = Some(v),
        ("category", RuleResult::String(Some(v))) => metadata.category = Some(v),
        ("required_secrets", RuleResult::StringSeq(seq)) => {
            metadata.required_secrets = seq;
        }
        (other, RuleResult::String(Some(v))) => {
            metadata.extra.insert(other.to_string(), Value::String(v));
        }
        (other, RuleResult::StringSeq(seq)) => {
            metadata.extra.insert(
                other.to_string(),
                Value::Array(seq.into_iter().map(Value::String).collect()),
            );
        }
        (_, RuleResult::String(None)) => {}
    }
}

fn extract_string_seq_from_value(parsed: &Value, key: &str) -> Vec<String> {
    parsed
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn extract_string_from_value(parsed: &Value, key: &str) -> Option<String> {
    let val = parsed.get(key)?;
    match val {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// A single extension entry within a `KindSchema`.
///
/// Captures the file extension, its metadata parser, and its
/// signature embedding format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionSpec {
    /// File extension including the dot, e.g. `".py"`, `".md"`
    pub ext: String,
    /// Canonical parser tool ref, e.g.
    /// `"parser:rye/core/python/ast"`. The boot validator
    /// guarantees this resolves through `ParserRegistry`.
    pub parser: String,
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
    /// Extract a scalar string from a key path in the parsed document
    Path { key: String },
    /// Extract a `Vec<String>` from a key path in the parsed document.
    /// The value at the path must be an array; non-string entries are
    /// dropped silently. Used for typed-list metadata fields like
    /// `required_secrets` so engine code does NOT special-case
    /// specific field names.
    PathStringSeq { key: String },
}

/// Path-anchoring constraint declared on a metadata rule.
///
/// The validator (`validate_metadata_anchoring`) consults this on each
/// rule to enforce that the extracted value matches the file's
/// on-disk location. Closed enum — extending it is a deliberate schema
/// change, never a silent fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    /// Extracted value must equal `file_path.file_stem()`.
    Filename,
    /// Extracted value must equal the file's directory components
    /// relative to `<ai_root>/<kind.directory>` joined by `/`. Empty
    /// string when the file lives directly under the kind directory.
    Path,
}

/// One entry in a kind schema's `metadata.rules` mapping.
///
/// `extractor` is what produces the field value from the parsed
/// document. `required` and `match_kind` are the path-anchoring
/// constraints; `apply_extraction_rules` ignores them, but
/// `validate_metadata_anchoring` enforces them at item load time.
///
/// Per-rule attrs live on the schema rather than in Rust so each
/// kind decides which fields are anchored without engine code
/// hardcoding field names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataRule {
    pub extractor: ExtractionRule,
    /// When `true`, the extractor MUST produce a non-empty scalar
    /// (or non-empty string sequence) — absence is a hard failure
    /// from `validate_metadata_anchoring`.
    pub required: bool,
    /// When `Some`, the extracted scalar must equal the on-disk
    /// anchor identified by the variant.
    pub match_kind: Option<MatchKind>,
}

/// Failure produced by the path-anchoring validator. The validator
/// stops at the first violation it finds — the engine does NOT carry
/// a vector of issues. One violation is the failure.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum MetadataAnchoringError {
    /// `required: true` rule found no value in the parsed document
    /// (or filename rule had a non-UTF-8 stem).
    #[error(
        "required metadata field `{field}` missing on `{}`",
        file.display()
    )]
    MissingRequired {
        file: PathBuf,
        field: String,
    },

    /// `match: filename` rule produced a value that disagrees with
    /// the file stem.
    #[error(
        "metadata field `{field}` does not match filename: \
         metadata says `{metadata_value}` but filename is `{filename}` (`{}`)",
        file.display()
    )]
    FilenameMismatch {
        file: PathBuf,
        field: String,
        metadata_value: String,
        filename: String,
    },

    /// `match: path` rule produced a value that disagrees with the
    /// path-relative-to-kind-directory.
    #[error(
        "metadata field `{field}` does not match path: \
         metadata says `{metadata_value}` but file is in `{path_value}` (`{}`)",
        file.display()
    )]
    PathMismatch {
        file: PathBuf,
        field: String,
        metadata_value: String,
        path_value: String,
    },

    /// File path is not under `<ai_root>/<kind_directory>`. Should
    /// never happen for items resolved through the engine's normal
    /// resolution paths — surfaces here only if the validator is
    /// invoked with a malformed `(ai_root, kind_directory, file_path)`
    /// triple.
    #[error(
        "file `{}` is not under expected kind directory `{}`",
        file.display(),
        expected_base.display()
    )]
    OutsideKindDirectory {
        file: PathBuf,
        expected_base: PathBuf,
    },
}

/// Run the path-anchoring validator for one item.
///
/// Iterates `rules` and enforces the per-rule `required` and
/// `match_kind` attrs declared on the kind schema. The engine does NOT
/// hardcode which fields are required or anchored; that is entirely
/// data-driven.
///
/// `kind_directory` is the kind's `.ai/` subdirectory (e.g. `tools`,
/// `directives`). `ai_root` is the `.ai/` directory under which the
/// file lives — the one the resolver picked the file out of (project,
/// user, or a system bundle). `file_path` is the absolute path to the
/// item.
///
/// Returns the FIRST violation encountered (stable rule ordering by
/// field name) — fail-loud, no aggregation.
pub fn validate_metadata_anchoring(
    parsed: &Value,
    rules: &HashMap<String, MetadataRule>,
    kind_directory: &str,
    ai_root: &Path,
    file_path: &Path,
) -> Result<(), MetadataAnchoringError> {
    // Stable iteration order so the first-failure error is deterministic.
    let mut field_names: Vec<&String> = rules.keys().collect();
    field_names.sort();

    for field in field_names {
        let rule = &rules[field];
        let result = extract_rule_value(&rule.extractor, parsed, file_path);

        // Required check: extractor must produce a value. An empty
        // string IS a value (it's the legal path-component for items
        // at the root of the kind directory) — only `None` and empty
        // seq count as missing.
        if rule.required {
            let present = match &result {
                RuleResult::String(s) => s.is_some(),
                RuleResult::StringSeq(seq) => !seq.is_empty(),
            };
            if !present {
                return Err(MetadataAnchoringError::MissingRequired {
                    file: file_path.to_path_buf(),
                    field: field.clone(),
                });
            }
        }

        // Match check: applies to scalar string rules only. Seq rules
        // can't be anchored to filename/path; the schema parser
        // rejects that combination at load time.
        if let Some(match_kind) = rule.match_kind {
            let metadata_value = match &result {
                RuleResult::String(Some(s)) => s.clone(),
                RuleResult::String(None) => {
                    // Not present and not required → nothing to compare
                    if !rule.required {
                        continue;
                    }
                    // required + missing was already returned above
                    unreachable!("missing-required handled above");
                }
                RuleResult::StringSeq(_) => {
                    // Schema parser rejects seq+match — defensive skip.
                    continue;
                }
            };

            match match_kind {
                MatchKind::Filename => {
                    let filename = file_path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string();
                    if metadata_value != filename {
                        return Err(MetadataAnchoringError::FilenameMismatch {
                            file: file_path.to_path_buf(),
                            field: field.clone(),
                            metadata_value,
                            filename,
                        });
                    }
                }
                MatchKind::Path => {
                    let expected_base = ai_root.join(kind_directory);
                    let parent = file_path.parent().unwrap_or(file_path);
                    let relative = match parent.strip_prefix(&expected_base) {
                        Ok(r) => r,
                        Err(_) => {
                            return Err(MetadataAnchoringError::OutsideKindDirectory {
                                file: file_path.to_path_buf(),
                                expected_base,
                            });
                        }
                    };
                    let path_value = relative
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("/");
                    if metadata_value != path_value {
                        return Err(MetadataAnchoringError::PathMismatch {
                            file: file_path.to_path_buf(),
                            field: field.clone(),
                            metadata_value,
                            path_value,
                        });
                    }
                }
            }
        }
    }

    Ok(())
}

/// Runtime-handler configuration for a kind. Declares which top-level
/// YAML blocks on items of this kind are claimed by which runtime
/// handler (`runtime.handlers`), and which keys the engine
/// deliberately ignores during runtime-block dispatch
/// (`runtime.ignored_keys`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeHandlerDecl {
    /// Handler key — must match a `RuntimeHandler::key()` string
    /// registered in the `RuntimeHandlerRegistry`.
    #[serde(rename = "type")]
    pub type_: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSpec {
    #[serde(default)]
    pub handlers: Vec<RuntimeHandlerDecl>,
    /// Top-level keys present on items that are deliberately not
    /// runtime blocks (metadata, header fields, etc.). Engine skips
    /// these during runtime-handler dispatch.
    #[serde(default)]
    pub ignored_keys: Vec<String>,
}

/// How a kind terminates dispatch. Two variants only — `InProcess` for
/// daemon-owned Rust handlers, `Subprocess` for everything that spawns a
/// child binary. The child's envelope shape is determined by the
/// `protocol_ref` field, which points into the `ProtocolRegistry`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TerminatorDecl {
    /// Daemon calls a registered Rust fn in a named registry.
    InProcess {
        registry: InProcessRegistryKind,
    },
    /// Daemon spawns a child binary; envelope shape comes from the
    /// referenced protocol descriptor.
    Subprocess {
        /// Canonical ref into ProtocolRegistry, e.g.
        /// "protocol:rye/core/runtime_v1".
        #[serde(rename = "protocol")]
        protocol_ref: String,
    },
}

/// Closed enum of named in-process handler registries. Single variant in V5.3
/// — additional registries (parsers, composers) are deferred per
/// docs/future/resolution-pipeline-advanced.md.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum InProcessRegistryKind {
    Services,
}

/// Mechanism by which a non-terminating, non-aliased kind delegates
/// dispatch to another item. Closed enum — adding a new variant is a
/// real architectural change. Currently the only mechanism is
/// `runtime_registry`: ask `RuntimeRegistry::lookup_for(serves_kind)`
/// for the default runtime serving this kind, then continue the
/// dispatch loop on the returned canonical ref.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "via", rename_all = "snake_case")]
pub enum DelegationVia {
    /// Look the next hop up in the runtime registry by `serves_kind`
    /// (defaulting to this schema's own kind name when omitted on the
    /// schema).
    RuntimeRegistry {
        /// Override the kind key used for the registry lookup. When
        /// `None`, the dispatcher uses the schema's own kind. Allows a
        /// schema to delegate "as if it were another kind" without
        /// being one — rarely needed but explicit.
        #[serde(default)]
        serves_kind: Option<String>,
    },
}

/// Explicit delegation declaration on a kind schema. When present,
/// the dispatch loop is allowed to perform a non-terminating,
/// non-aliased hop via the declared mechanism. Absence means the
/// dispatcher will NEVER consult the runtime registry on behalf of
/// this kind — silent fallback is gone.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DelegationSpec {
    #[serde(flatten)]
    pub via: DelegationVia,
}

/// Execution configuration for a kind (resolution pipeline + aliases).
/// Only kinds with an execution block can be executed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSchema {
    /// Shorthand resolution for @ refs in this kind's chains.
    /// Aliases compose recursively (capped by alias_max_depth).
    #[serde(default)]
    pub aliases: HashMap<String, String>,
    /// Hard cap for recursive alias expansion (default 8).
    #[serde(default = "default_alias_depth")]
    pub alias_max_depth: usize,
    /// Ordered preprocessing pipeline run before dispatch.
    #[serde(default)]
    pub resolution: Vec<ResolutionStepDecl>,
    /// How this kind terminates dispatch. Optional because a kind may
    /// instead dispatch by alias chain (the `aliases` field above) or
    /// by explicit delegation (the `delegate` field below). Exactly
    /// one of {terminator, terminating alias chain, delegate} must be
    /// declared on any executable schema; pure absence is a load-time
    /// error.
    ///
    /// There is NO silent fallback to a legacy dispatch path. If a
    /// schema declares `execution:` with none of the three, that is a
    /// schema error caught at load time.
    ///
    /// The nested form is parsed natively by serde (tagged enum on
    /// `kind`). No manual parse step.
    #[serde(default)]
    pub terminator: Option<TerminatorDecl>,
    /// Explicit delegation declaration. When `Some`, the dispatch
    /// loop is permitted to consult the declared mechanism (today:
    /// `RuntimeRegistry::lookup_for`). When `None`, the dispatcher
    /// will NEVER consult the registry on behalf of this kind — the
    /// "no terminator + no alias = look it up in the registry"
    /// implicit fallback was removed in favor of this explicit field.
    #[serde(default)]
    pub delegate: Option<DelegationSpec>,
    /// Schema-declared thread-profile name (looked up in the daemon's
    /// `KindProfileRegistry`). The terminator dispatchers
    /// (`dispatch_subprocess`, `dispatch_managed_subprocess`) read this
    /// instead of hardcoding profile names — V5.4 SSE adds a streaming
    /// runtime profile by changing the schema, not the dispatch code.
    #[serde(default)]
    pub thread_profile: Option<String>,
}

fn default_alias_depth() -> usize {
    8
}

/// Complete schema for a single item kind, loaded from a kind schema
/// YAML. One struct per kind — no parallel maps, no split state.
#[derive(Debug, Clone)]
pub struct KindSchema {
    /// The `.ai/` subdirectory name, e.g. `"tools"`, `"directives"`
    pub directory: String,
    /// Ordered extension specs — extension priority during resolution
    /// is the order declared in the schema
    pub extensions: Vec<ExtensionSpec>,
    /// Data-driven extraction rules: output field name → rule
    /// (extractor + per-rule path-anchoring attrs).
    pub extraction_rules: HashMap<String, MetadataRule>,
    /// Execution configuration (resolution pipeline + aliases).
    /// `None` if this kind is not executable (e.g., config kind).
    pub execution: Option<ExecutionSchema>,
    /// Declared shape contract on the parsed `Value` that the parser
    /// must produce for this kind's composer. REQUIRED on every
    /// kind schema. Kinds with no field-level constraint at boot
    /// must declare an explicit empty contract
    /// (`root_type: mapping, required: {}`) — absence is no longer a
    /// silent default but a deliberate, reviewed declaration.
    /// The boot validator runs `is_satisfied_by` against each
    /// extension parser's `output_schema` and aggregates ALL
    /// violations.
    pub composed_value_contract: ValueShape,
    /// Native composer handler ID this kind binds to (e.g.
    /// `"handler:rye/core/extends-chain"`,
    /// `"handler:rye/core/identity"`). REQUIRED
    /// on every kind schema — there is no silent "no composer" path.
    /// The boot validator guarantees this resolves through
    /// the `HandlerRegistry`; `ComposerRegistry::from_kinds`
    /// uses this to bind kind→composer data-drivenly.
    pub composer: String,
    /// Opaque-to-the-engine composer-config blob, mirroring how
    /// `ParserDescriptor::parser_config` is opaque to the parser
    /// dispatcher. The kind's composer handler validates and consumes
    /// it. REQUIRED at the schema layer but defaults to `Value::Null`
    /// when the YAML omits the block — composers that take no config
    /// (e.g. the `handler:rye/core/identity` composer binary) accept Null.
    pub composer_config: Value,
    /// Runtime-handler dispatch declaration (which YAML blocks on
    /// items of this kind are runtime blocks, plus the ignore list).
    /// `None` for kinds whose items are never compiled into a
    /// `SubprocessSpec` (e.g. config kinds).
    pub runtime: Option<RuntimeSpec>,
    /// **Launching-side** declaration: kinds whose items the daemon
    /// should bake into `LaunchEnvelope.inventory[<kind>]` whenever
    /// THIS kind is the executor of a `/execute` request. Empty when
    /// the kind doesn't need any pre-baked inventory (most kinds).
    /// `directive` declares `inventory_kinds: [tool]`; future runtimes
    /// add their own without the daemon needing kind-specific code.
    pub inventory_kinds: Vec<String>,
    /// **Inventoried-side** declaration: candidate keys (in priority
    /// order) the engine should probe in a parsed item's body to fill
    /// `ItemDescriptor.schema`. The first key whose value is non-null
    /// wins. Empty when this kind has no schema field (e.g. `config`,
    /// `directive`, `parser`). Tools typically declare
    /// `inventory_schema_keys: [input_schema, parameters, config_schema]`.
    pub inventory_schema_keys: Vec<String>,
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
            parser: spec.parser.clone(),
            signature: spec.signature.clone(),
        })
    }

    /// Get the execution schema (aliases + resolution pipeline).
    /// Returns `None` if this kind is not executable.
    pub fn execution(&self) -> Option<&ExecutionSchema> {
        self.execution.as_ref()
    }

    /// Whether this kind is executable (has an `execution` block).
    pub fn is_executable(&self) -> bool {
        self.execution.is_some()
    }

    /// Get the runtime-handler dispatch spec for this kind.
    pub fn runtime(&self) -> Option<&RuntimeSpec> {
        self.runtime.as_ref()
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

    /// Load the kind registry from node-bundle kind schema search paths.
    ///
    /// Scans `{kind}/*.kind-schema.yaml` files within each search root.
    /// Uses raw filesystem scanning — no item resolution dependency.
    /// Every kind schema must be signed and verified against the trust
    /// store. Unsigned or tampered schemas cause the entire load to
    /// fail.
    ///
    /// Kind schemas are node-tier items (live under
    /// `<bundle-root>/.ai/node/engine/kinds/`); they do NOT participate
    /// in the project/user/system three-tier resolution that operator-
    /// edited items use. Multiple roots are supported only because more
    /// than one node bundle may ship kind schemas — there is no
    /// per-project override path, and the first-found bundle wins on a
    /// clash (stable ordering by `search_roots` argument).
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
            load_schemas_from_dir(root, &mut schemas, &mut fingerprint_data, trust_store)?;
        }

        let fingerprint = lillux::cas::sha256_hex(&fingerprint_data);

        Ok(Self {
            schemas,
            fingerprint,
        })
    }

    /// Get the full schema for a kind.
    pub fn get(&self, kind: &str) -> Option<&KindSchema> {
        let found = self.schemas.get(kind);
        if found.is_none() {
            tracing::trace!(kind = %kind, registered = self.schemas.len(), "kind registry miss");
        }
        found
    }

    /// Check whether a kind is registered.
    pub fn contains(&self, kind: &str) -> bool {
        self.schemas.contains_key(kind)
    }

    /// Get the directory name for a kind.
    pub fn directory(&self, kind: &str) -> Option<&str> {
        self.schemas.get(kind).map(|s| s.directory.as_str())
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
        let found = self.schemas.get(kind)?.spec_for(ext);
        tracing::trace!(kind = %kind, ext = %ext, hit = found.is_some(), "kind registry spec_for");
        found
    }

    /// Build a `ResolvedSourceFormat` from a matched kind + extension.
    pub fn resolved_format_for(&self, kind: &str, ext: &str) -> Option<ResolvedSourceFormat> {
        let found = self.schemas.get(kind)?.resolved_format_for(ext);
        tracing::trace!(kind = %kind, ext = %ext, hit = found.is_some(), "kind registry resolved_format_for");
        found
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
            // First-found wins. Shadowing check happens BEFORE
            // verification: a schema that's already claimed the kind
            // from an earlier `search_roots` entry never gets used
            // downstream, so its shadowed copy doesn't need to verify.
            if schemas.contains_key(&kind_name) {
                tracing::debug!(
                    kind = %kind_name,
                    path = %yaml_path.display(),
                    "skipped shadowed kind schema (earlier root claimed this kind)"
                );
                continue;
            }

            let parsed = load_and_verify_kind_schema(&yaml_path, trust_store)?;

            schemas.insert(kind_name.clone(), parsed);
            if let Ok(content) = std::fs::read(&yaml_path) {
                fingerprint_data.extend_from_slice(&content);
            }
            tracing::debug!(kind = %kind_name, path = %yaml_path.display(), "loaded kind schema");
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
                    canonical_ref: format!("node:{}", infer_node_id(yaml_path)),
                    expected: header.content_hash,
                    actual: actual_hash,
                });
            }

            let signer = trust_store.get(&header.signer_fingerprint).ok_or_else(|| {
                EngineError::UntrustedSigner {
                    canonical_ref: format!("node:{}", infer_node_id(yaml_path)),
                    fingerprint: header.signer_fingerprint.clone(),
                }
            })?;

            if !lillux::signature::verify_signature(
                &header.content_hash,
                &header.signature_b64,
                &signer.verifying_key,
            ) {
                return Err(EngineError::SignatureVerificationFailed {
                    canonical_ref: format!("node:{}", infer_node_id(yaml_path)),
                    reason: "Ed25519 signature verification failed".into(),
                });
            }
        }
        None => {
            return Err(EngineError::SignatureMissing {
                canonical_ref: format!("node:{}", infer_node_id(yaml_path)),
            });
        }
    }

    parse_kind_schema_content(&yaml_path.display().to_string(), &content)
}

/// Reverse-map a kind-schema filesystem path to a node item ID.
///
/// Strips the `.ai/node/` prefix and `.kind-schema.yaml` suffix.
/// Example: `.ai/node/engine/kinds/tool/tool.kind-schema.yaml` →
/// `engine/kinds/tool/tool`
fn infer_node_id(yaml_path: &Path) -> String {
    let path_str = yaml_path.to_string_lossy();
    let needle = ".ai/node/";
    if let Some(idx) = path_str.find(needle) {
        let after = &path_str[idx + needle.len()..];
        if let Some(stripped) = after.strip_suffix(".kind-schema.yaml") {
            return stripped.to_string();
        }
    }
    // Path doesn't contain .ai/node/ — use kind_dir/filename_stem
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

    let execution = parse_execution_schema(&data, display)?;

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

        let parser = entry
            .get("parser")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned())
            .ok_or_else(|| EngineError::SchemaLoaderError {
                reason: format!("{display}: {entry_label} missing `parser`"),
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
                parser: parser.clone(),
                signature: signature.clone(),
            });
        }
    }

    let extraction_rules = parse_extraction_rules(&data, display)?;

    let composed_value_contract = match data.get("composed_value_contract") {
        Some(v) if !v.is_null() => serde_yaml::from_value::<ValueShape>(v.clone()).map_err(
            |e| EngineError::SchemaLoaderError {
                reason: format!("{display}: invalid `composed_value_contract`: {e}"),
            },
        )?,
        _ => {
            return Err(EngineError::SchemaLoaderError {
                reason: format!(
                    "{display}: missing required field `composed_value_contract` \
                    (declare `root_type: mapping, required: {{}}` for kinds with no \
                    boot-level shape constraint — absence is not a silent default)"
                ),
            });
        }
    };

    let composer = data
        .get("composer")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .ok_or_else(|| EngineError::SchemaLoaderError {
            reason: format!(
                "{display}: kind schema missing required field `composer` \
                 (declare a native composer handler ID, e.g. \
                 `composer: handler:rye/core/identity` for kinds with no \
                 composition, or `composer: handler:rye/core/extends-chain`)"
            ),
        })?;

    // `composer_config` is opaque to the engine — composer handlers
    // own validation. Absence ⇒ Value::Null (handlers like
    // the `handler:rye/core/identity` composer binary explicitly accept Null).
    let composer_config = match data.get("composer_config") {
        Some(v) => yaml_to_json(v.clone()).map_err(|e| EngineError::SchemaLoaderError {
            reason: format!("{display}: invalid `composer_config`: {e}"),
        })?,
        None => Value::Null,
    };

    let runtime = match data.get("runtime") {
        Some(v) if !v.is_null() => Some(
            serde_yaml::from_value::<RuntimeSpec>(v.clone()).map_err(|e| {
                EngineError::SchemaLoaderError {
                    reason: format!("{display}: invalid `runtime` block: {e}"),
                }
            })?,
        ),
        _ => None,
    };

    // `inventory_kinds` (launching-side) and `inventory_schema_keys`
    // (inventoried-side) are both optional — most kinds need neither.
    // When present, they MUST be sequences of plain strings; a wrong
    // shape is a hard schema error rather than a silent default.
    let inventory_kinds = parse_optional_string_seq(&data, "inventory_kinds", display)?;
    let inventory_schema_keys =
        parse_optional_string_seq(&data, "inventory_schema_keys", display)?;

    Ok(KindSchema {
        directory,
        extensions,
        extraction_rules,
        execution,
        composed_value_contract,
        composer,
        composer_config,
        runtime,
        inventory_kinds,
        inventory_schema_keys,
    })
}

fn parse_optional_string_seq(
    data: &serde_yaml::Value,
    key: &str,
    display: &str,
) -> Result<Vec<String>, EngineError> {
    match data.get(key) {
        None | Some(serde_yaml::Value::Null) => Ok(Vec::new()),
        Some(serde_yaml::Value::Sequence(seq)) => {
            let mut out = Vec::with_capacity(seq.len());
            for (i, v) in seq.iter().enumerate() {
                match v.as_str() {
                    Some(s) => out.push(s.to_owned()),
                    None => {
                        return Err(EngineError::SchemaLoaderError {
                            reason: format!(
                                "{display}: `{key}[{i}]` must be a string, got {v:?}"
                            ),
                        })
                    }
                }
            }
            Ok(out)
        }
        Some(other) => Err(EngineError::SchemaLoaderError {
            reason: format!(
                "{display}: `{key}` must be a sequence of strings, got {other:?}"
            ),
        }),
    }
}

fn yaml_to_json(value: serde_yaml::Value) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|e| e.to_string())
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
) -> Result<HashMap<String, MetadataRule>, EngineError> {
    let mapping = match data
        .get("metadata")
        .and_then(|v| v.get("rules"))
        .and_then(|v| v.as_mapping())
    {
        Some(m) => m,
        None => return Ok(HashMap::new()),
    };

    // Whitelist of accepted keys per rule. Anything else is a typed
    // schema error — fail-loud, no silent drops.
    const ACCEPTED_KEYS: &[&str] = &["from", "value", "key", "required", "match"];

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

        // Reject unknown keys before any other parsing — keeps the
        // schema closed without sprinkling per-key validation.
        for (rk, _) in rule_map {
            let name = rk.as_str().ok_or_else(|| EngineError::SchemaLoaderError {
                reason: format!(
                    "{display}: non-string key in metadata.rules.{field}"
                ),
            })?;
            if !ACCEPTED_KEYS.contains(&name) {
                return Err(EngineError::SchemaLoaderError {
                    reason: format!(
                        "{display}: metadata.rules.{field} unknown key `{name}` \
                         (accepted: {ACCEPTED_KEYS:?})"
                    ),
                });
            }
        }

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

        let extractor = match rule_type.as_str() {
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
            "path_string_seq" => {
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
                            "{display}: metadata.rules.{field} from=path_string_seq requires `key`"
                        ),
                    })?;
                ExtractionRule::PathStringSeq { key }
            }
            other => {
                return Err(EngineError::SchemaLoaderError {
                    reason: format!("{display}: metadata.rules.{field} unknown from `{other}`"),
                });
            }
        };

        // Per-rule path-anchoring attrs.
        let required = rule_map
            .iter()
            .find_map(|(rk, rv)| {
                if rk.as_str() == Some("required") {
                    Some(rv)
                } else {
                    None
                }
            })
            .map(|v| {
                v.as_bool().ok_or_else(|| EngineError::SchemaLoaderError {
                    reason: format!(
                        "{display}: metadata.rules.{field}.required must be a bool"
                    ),
                })
            })
            .transpose()?
            .unwrap_or(false);

        let match_kind = match rule_map
            .iter()
            .find_map(|(rk, rv)| {
                if rk.as_str() == Some("match") {
                    Some(rv)
                } else {
                    None
                }
            }) {
            Some(v) => {
                let s = v.as_str().ok_or_else(|| EngineError::SchemaLoaderError {
                    reason: format!(
                        "{display}: metadata.rules.{field}.match must be a string \
                         (filename | path)"
                    ),
                })?;
                let mk = match s {
                    "filename" => MatchKind::Filename,
                    "path" => MatchKind::Path,
                    other => {
                        return Err(EngineError::SchemaLoaderError {
                            reason: format!(
                                "{display}: metadata.rules.{field}.match unknown value \
                                 `{other}` (accepted: filename, path)"
                            ),
                        });
                    }
                };
                // Anchoring on a sequence rule is incoherent — the
                // anchor is always a scalar (filename stem or path
                // string). Fail-loud rather than silently skipping
                // at validate time.
                if matches!(extractor, ExtractionRule::PathStringSeq { .. }) {
                    return Err(EngineError::SchemaLoaderError {
                        reason: format!(
                            "{display}: metadata.rules.{field} cannot declare \
                             `match` on a `path_string_seq` extractor — anchors \
                             require a scalar value"
                        ),
                    });
                }
                Some(mk)
            }
            None => None,
        };

        rules.insert(
            field,
            MetadataRule {
                extractor,
                required,
                match_kind,
            },
        );
    }

    Ok(rules)
}

/// Parse the `execution.aliases` block from a kind schema.
///
/// Maps `@`-prefixed shorthand names to canonical refs.
/// If no `execution` block exists, returns empty HashMap (kind is not executable).
fn parse_execution_schema(
    data: &serde_yaml::Value,
    display: &str,
) -> Result<Option<ExecutionSchema>, EngineError> {
    let execution_value = match data.get("execution") {
        Some(v) => v,
        None => return Ok(None),
    };

    let _ = execution_value
        .as_mapping()
        .ok_or_else(|| EngineError::SchemaLoaderError {
            reason: format!("{display}: `execution` must be a mapping"),
        })?;

    let mut aliases = HashMap::new();
    if let Some(aliases_value) = execution_value.get("aliases") {
        if let Some(aliases_mapping) = aliases_value.as_mapping() {
            for (k, v) in aliases_mapping {
                if let (Some(key), Some(val)) = (k.as_str(), v.as_str()) {
                    aliases.insert(key.to_owned(), val.to_owned());
                }
            }
        }
    }

    let alias_max_depth = execution_value
        .get("alias_max_depth")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(8);

    let mut resolution = Vec::new();
    if let Some(res_value) = execution_value.get("resolution") {
        if let Some(res_seq) = res_value.as_sequence() {
            for item in res_seq {
                let step: ResolutionStepDecl = serde_yaml::from_value(item.clone())
                    .map_err(|e| EngineError::SchemaLoaderError {
                        reason: format!("{display}: invalid resolution step: {e}"),
                    })?;
                resolution.push(step);
            }
        }
    }

    let terminator = if let Some(t_value) = execution_value.get("terminator") {
        Some(serde_yaml::from_value(t_value.clone()).map_err(|e| {
            EngineError::SchemaLoaderError {
                reason: format!("{display}: invalid terminator declaration: {e}"),
            }
        })?)
    } else {
        None
    };

    let delegate = parse_delegation_spec(execution_value, display)?;

    let thread_profile = execution_value
        .get("thread_profile")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());

    // S1: load-time non-actionable schema rejection. A kind that
    // declares `execution:` MUST declare at least one routing
    // primitive the dispatcher can act on:
    //   * a `terminator` (InProcess / Subprocess)
    //   * one or more `aliases` (which may include an `@<own_kind>`
    //     entry the dispatch loop walks via `FollowAlias`, OR
    //     `@<other>` entries used by `plan_builder` for tool-chain
    //     `executor_id` resolution — both are legitimate uses of the
    //     same field)
    //   * a `delegate` block (explicit registry/etc. routing)
    //
    // Pre-V5.4 the dispatcher silently consulted
    // `RuntimeRegistry::lookup_for` when a schema had a
    // `thread_profile` but no terminator/aliases. That implicit
    // fallback is gone: a schema must opt in to registry routing via
    // `delegate: { via: runtime_registry }`.
    //
    // Note: we do NOT require mutual exclusion among the three. The
    // tool kind, for example, declares `terminator: subprocess` AND
    // `aliases: { "@subprocess": ... }` — the alias serves the
    // tool-chain `executor_id` mechanism in `plan_builder`, not the
    // dispatch loop. Precedence in the dispatch loop is fixed
    // (terminator > `@<own_kind>` alias > delegate); a schema author
    // declaring more than one is exercising different mechanisms,
    // not creating dispatcher ambiguity.
    let has_routing_primitive = terminator.is_some()
        || !aliases.is_empty()
        || delegate.is_some();
    if !has_routing_primitive {
        return Err(EngineError::SchemaLoaderError {
            reason: format!(
                "{display}: kind declares `execution:` block but none of \
                 `terminator`, `aliases`, or `delegate` — schema cannot be \
                 dispatched. Add a terminator, an `@<kind>` alias chain, or \
                 `delegate: {{ via: runtime_registry }}`."
            ),
        });
    }

    Ok(Some(ExecutionSchema {
        aliases,
        alias_max_depth,
        resolution,
        terminator,
        delegate,
        thread_profile,
    }))
}

/// Parse the `execution.delegate` block from a kind schema. Returns
/// `None` when no delegation is declared. Closed-enum on `via`: any
/// mechanism name not in the Rust enum is a hard parse error.
fn parse_delegation_spec(
    execution_value: &serde_yaml::Value,
    display: &str,
) -> Result<Option<DelegationSpec>, EngineError> {
    let Some(d_value) = execution_value.get("delegate") else {
        return Ok(None);
    };
    let mapping = d_value
        .as_mapping()
        .ok_or_else(|| EngineError::SchemaLoaderError {
            reason: format!("{display}: `execution.delegate` must be a mapping"),
        })?;
    let via_str = mapping
        .get(serde_yaml::Value::String("via".to_string()))
        .and_then(|v| v.as_str())
        .ok_or_else(|| EngineError::SchemaLoaderError {
            reason: format!(
                "{display}: `execution.delegate` requires a `via` field \
                 (known: runtime_registry)"
            ),
        })?;
    let via = match via_str {
        "runtime_registry" => {
            let serves_kind = mapping
                .get(serde_yaml::Value::String("serves_kind".to_string()))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            DelegationVia::RuntimeRegistry { serves_kind }
        }
        other => {
            return Err(EngineError::SchemaLoaderError {
                reason: format!(
                    "{display}: unknown delegation mechanism `{other}` \
                     (known: runtime_registry)"
                ),
            });
        }
    };
    Ok(Some(DelegationSpec { via }))
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
    parser: parser:rye/core/python/ast
    signature:
      prefix: \"#\"
      after_shebang: true
  - extensions: [\".yaml\", \".yml\"]
    parser: parser:rye/core/yaml/yaml
    signature:
      prefix: \"#\"
  - extensions: [\".js\", \".ts\"]
    parser: parser:rye/core/javascript/javascript
    signature:
      prefix: \"//\"
  - extensions: [\".sh\"]
    parser: parser:rye/core/python/ast
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
  aliases:
    \"@directive\": \"tool:rye/directive-runtime/runtime\"
formats:
  - extensions: [\".md\"]
    parser: parser:rye/core/markdown/directive
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
execution:
  thread_profile: directive_run
  delegate:
    via: runtime_registry
  resolution:
    - step: resolve_extends_chain
    - step: resolve_references
formats:
  - extensions: [\".md\"]
    parser: parser:rye/core/markdown/directive
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
        // Inject the now-mandatory composed_value_contract for tests
        // that only care about other fields. Tests that explicitly
        // exercise contract presence/absence include their own block.
        let yaml = if yaml.contains("composed_value_contract") {
            yaml.to_string()
        } else {
            format!("{yaml}composed_value_contract:\n  root_type: mapping\n  required: {{}}\n")
        };
        let yaml = if yaml.contains("composer:") {
            yaml
        } else {
            format!("{yaml}composer: handler:rye/core/identity\n")
        };
        let signed = lillux::signature::sign_content(&yaml, sk, "#", None);
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

    const SERVICE_SCHEMA: &str = "\
location:
  directory: services
formats:
  - extensions: [\".yaml\", \".yml\"]
    parser: parser:rye/core/yaml/yaml
    signature:
      prefix: \"#\"
composer: handler:rye/core/identity
composed_value_contract:
  root_type: mapping
  required: {}
metadata:
  rules:
    endpoint:
      from: path
      key: endpoint
    required_caps:
      from: path_string_seq
      key: required_caps
";

    fn write_service_schema(dir: &Path, sk: &SigningKey) {
        sign_and_write_schema(dir, "service", SERVICE_SCHEMA, sk);
    }

    #[test]
    fn load_service_kind_schema() {
        let tmp = tempdir();
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);
        write_service_schema(&tmp, &sk);

        let reg = KindRegistry::load_base(&[tmp.clone()], &ts).unwrap();

        let svc = reg.get("service").expect("service kind should be registered");
        assert_eq!(svc.directory, "services");
        let exts = svc.extension_strs();
        assert!(exts.contains(&".yaml"));
        assert!(exts.contains(&".yml"));

        // Service has no runtime handlers (daemon-dispatched, not subprocess)
        assert!(svc.runtime.is_none());

        // Service has no execution aliases (no resolve_extends_chain)
        assert!(svc.execution.is_none());
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
        assert!(tool.execution.as_ref().map_or(true, |e| e.aliases.is_empty()));
        let tool_exts = tool.extension_strs();
        assert!(tool_exts.contains(&".py"));
        assert!(tool_exts.contains(&".ts"));
        assert!(tool_exts.contains(&".sh"));

        // Directive schema
        let dir = reg.get("directive").unwrap();
        assert_eq!(dir.directory, "directives");
        assert_eq!(
            dir.execution.as_ref().and_then(|e| e.aliases.get("@directive")).map(|s| s.as_str()),
            Some("tool:rye/directive-runtime/runtime")
        );
        assert_eq!(dir.extension_strs(), vec![".md"]);

        // Parser lookups
        let py_spec = reg.spec_for("tool", ".py").unwrap();
        assert_eq!(py_spec.parser, "parser:rye/core/python/ast");

        let ts_spec = reg.spec_for("tool", ".ts").unwrap();
        assert_eq!(ts_spec.parser, "parser:rye/core/javascript/javascript");
        assert_eq!(ts_spec.signature.prefix, "//");
        assert!(!ts_spec.signature.after_shebang);

        let md_spec = reg.spec_for("directive", ".md").unwrap();
        assert_eq!(md_spec.parser, "parser:rye/core/markdown/directive");
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
            reg.get("directive").unwrap().execution.as_ref().and_then(|e| e.aliases.get("@directive")).map(|s| s.as_str()),
            Some("tool:rye/directive-runtime/runtime")
        );
        assert_eq!(
            reg.get("tool").unwrap().execution.as_ref().and_then(|e| e.aliases.get("@subprocess")).map(|s| s.as_str()),
            None
        );

        assert!(reg.contains("tool"));
        assert!(!reg.contains("nonexistent"));

        assert_eq!(reg.len(), 2);
        assert!(!reg.is_empty());
    }

    // Removed: `project_overlay_replaces_kind` — kind schemas now live
    // exclusively as a node kind under `.ai/node/engine/kinds/*`, so
    // there is no project-tier override path. The shadow semantics
    // (system wins on clash) are covered by `load_schemas_from_dir`'s
    // first-found-wins behavior; an explicit project-overlay-replace
    // test no longer reflects the architecture.

    #[test]
    fn resolved_format_for() {
        let tmp = tempdir();
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);
        write_tool_schema(&tmp, &sk);

        let reg = KindRegistry::load_base(&[tmp], &ts).unwrap();
        let fmt = reg.resolved_format_for("tool", ".py").unwrap();
        assert_eq!(fmt.extension, ".py");
        assert_eq!(fmt.parser, "parser:rye/core/python/ast");
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
            "location:\n  directory: tools\nformats:\n  - extensions: [\".py\"]\n    parser: parser:rye/core/python/ast\n    signature:\n      prefix: \"#\"\n",
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
    parser: parser:rye/core/python/ast
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
    fn reject_missing_parser() {
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
            matches!(err, EngineError::SchemaLoaderError { ref reason } if reason.contains("parser")),
            "expected parser error, got: {err:?}"
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
    parser: parser:rye/core/python/ast
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
            tool.extraction_rules.get("name").map(|r| &r.extractor),
            Some(&ExtractionRule::Filename)
        );
        assert_eq!(
            tool.extraction_rules.get("version").map(|r| &r.extractor),
            Some(&ExtractionRule::Path {
                key: "__version__".into()
            })
        );
        assert_eq!(
            tool.extraction_rules.get("executor_id").map(|r| &r.extractor),
            Some(&ExtractionRule::Path {
                key: "__executor_id__".into()
            })
        );

        // Directive schema has constant + path rules
        let dir = reg.get("directive").unwrap();
        assert_eq!(dir.extraction_rules.len(), 2);
        assert_eq!(
            dir.extraction_rules.get("executor_id").map(|r| &r.extractor),
            Some(&ExtractionRule::Constant {
                value: "native:directive_orchestrator".into()
            })
        );
        assert_eq!(
            dir.extraction_rules.get("version").map(|r| &r.extractor),
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
    parser: parser:rye/core/python/ast
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
            dir.execution.as_ref().map(|e| e.resolution.len()),
            Some(2)
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
        assert!(tool.execution.as_ref().map_or(true, |e| e.resolution.is_empty()));
    }

    // Removed: `project_overlay_replaces_resolution` — kind schemas are
    // node-tier items, not three-tier-resolved. There is no project
    // overlay path; `with_project_overlay` was deleted.

    fn parse_exec(yaml: &str) -> Result<Option<ExecutionSchema>, EngineError> {
        let v: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        parse_execution_schema(&v, "test.yaml")
    }

    #[test]
    fn execution_schema_parses_subprocess_terminator() {
        let yaml = "\
execution:
  terminator:
    kind: subprocess
    protocol: protocol:rye/core/opaque
  aliases:
    \"@subprocess\": \"tool:rye/core/subprocess/execute\"
";
        let exec = parse_exec(yaml).unwrap().expect("execution present");
        assert_eq!(
            exec.terminator,
            Some(TerminatorDecl::Subprocess {
                protocol_ref: "protocol:rye/core/opaque".into()
            })
        );
        assert_eq!(
            exec.aliases.get("@subprocess").map(|s| s.as_str()),
            Some("tool:rye/core/subprocess/execute")
        );
    }

    #[test]
    fn execution_schema_parses_in_process_terminator_with_services_registry() {
        let yaml = "\
execution:
  terminator:
    kind: in_process
    registry: services
";
        let exec = parse_exec(yaml).unwrap().expect("execution present");
        assert_eq!(
            exec.terminator,
            Some(TerminatorDecl::InProcess {
                registry: InProcessRegistryKind::Services
            })
        );
    }

    #[test]
    fn execution_schema_parses_subprocess_terminator_with_runtime_protocol() {
        let yaml = "\
execution:
  terminator:
    kind: subprocess
    protocol: protocol:rye/core/runtime_v1
";
        let exec = parse_exec(yaml).unwrap().expect("execution present");
        assert_eq!(
            exec.terminator,
            Some(TerminatorDecl::Subprocess {
                protocol_ref: "protocol:rye/core/runtime_v1".into()
            })
        );
    }

    #[test]
    fn execution_schema_terminator_field_optional() {
        // Terminator may be omitted when an alternative routing
        // primitive is present. Per S1, `delegate: { via: runtime_registry }`
        // is the explicit opt-in for runtime-registry routing — pre-V5.4
        // the dispatcher used a silent fallback when only `thread_profile`
        // was declared; that path is removed.
        let yaml = "\
execution:
  thread_profile: directive_run
  delegate:
    via: runtime_registry
";
        let exec = parse_exec(yaml).unwrap().expect("execution present");
        assert_eq!(exec.terminator, None);
        assert_eq!(exec.thread_profile.as_deref(), Some("directive_run"));
        assert!(exec.delegate.is_some(), "delegate must parse");
    }

    /// S1: `execution:` block with no terminator AND no aliases AND
    /// no `delegate` is non-actionable and MUST fail at load.
    #[test]
    fn execution_block_without_terminator_or_aliases_or_thread_profile_rejected_at_load() {
        let yaml = "\
execution:
  aliases: {}
  thread_profile: directive_run
";
        let err = parse_exec(yaml).expect_err("non-actionable schema must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("none of") && msg.contains("delegate"),
            "error must enumerate the missing routing primitives \
             (terminator/aliases/delegate), got: {msg}"
        );
    }

    /// Explicit-delegation parse: `delegate: { via: runtime_registry }`
    /// produces a `DelegationSpec` with the registry variant. Optional
    /// `serves_kind` defaults to `None` and is populated only when set.
    #[test]
    fn execution_schema_parses_delegate_runtime_registry() {
        let yaml = "\
execution:
  thread_profile: directive_run
  delegate:
    via: runtime_registry
";
        let exec = parse_exec(yaml).unwrap().expect("execution present");
        let delegation = exec.delegate.expect("delegate must parse");
        match delegation.via {
            DelegationVia::RuntimeRegistry { serves_kind } => {
                assert_eq!(serves_kind, None);
            }
        }
    }

    #[test]
    fn execution_schema_parses_delegate_with_serves_kind() {
        let yaml = "\
execution:
  thread_profile: graph_run
  delegate:
    via: runtime_registry
    serves_kind: directive
";
        let exec = parse_exec(yaml).unwrap().expect("execution present");
        let delegation = exec.delegate.expect("delegate must parse");
        match delegation.via {
            DelegationVia::RuntimeRegistry { serves_kind } => {
                assert_eq!(serves_kind.as_deref(), Some("directive"));
            }
        }
    }

    #[test]
    fn execution_schema_rejects_unknown_delegate_via() {
        let yaml = "\
execution:
  delegate:
    via: not_a_real_mechanism
";
        let err = parse_exec(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not_a_real_mechanism") && msg.contains("runtime_registry"),
            "unknown delegate.via must enumerate known mechanisms, got: {msg}"
        );
    }

    #[test]
    fn execution_schema_rejects_unknown_terminator_kind() {
        let yaml = "\
execution:
  terminator:
    kind: wasm_sandbox
    protocol: protocol:rye/core/opaque
";
        let err = parse_exec(yaml).unwrap_err();
        match err {
            EngineError::SchemaLoaderError { reason } => {
                assert!(
                    reason.contains("invalid terminator declaration"),
                    "unexpected error: {reason}"
                );
            }
            other => panic!("expected SchemaLoaderError, got {other:?}"),
        }
    }

    #[test]
    fn execution_schema_rejects_in_process_with_unknown_registry() {
        let yaml = "\
execution:
  terminator:
    kind: in_process
    registry: parsers
";
        let err = parse_exec(yaml).unwrap_err();
        match err {
            EngineError::SchemaLoaderError { reason } => {
                assert!(
                    reason.contains("invalid terminator declaration"),
                    "unexpected error: {reason}"
                );
            }
            other => panic!("expected SchemaLoaderError, got {other:?}"),
        }
    }

    #[test]
    fn execution_schema_rejects_subprocess_without_protocol() {
        let yaml = "\
execution:
  terminator:
    kind: subprocess
";
        let err = parse_exec(yaml).unwrap_err();
        match err {
            EngineError::SchemaLoaderError { reason } => {
                assert!(
                    reason.contains("invalid terminator declaration"),
                    "unexpected error: {reason}"
                );
            }
            other => panic!("expected SchemaLoaderError, got {other:?}"),
        }
    }

    #[test]
    fn infer_node_id_extracts_path() {
        let path = Path::new("/home/user/.ai/node/engine/kinds/tool/tool.kind-schema.yaml");
        assert_eq!(infer_node_id(path), "engine/kinds/tool/tool");
    }

    #[test]
    fn infer_node_id_no_ai_node_prefix() {
        let path = Path::new("/tmp/test_123/tool/tool.kind-schema.yaml");
        assert_eq!(infer_node_id(path), "engine/kinds/tool/tool");
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

    // ── Per-rule attrs (required + match) round-trip ───────────────

    #[test]
    fn metadata_rule_attrs_round_trip_from_schema_yaml() {
        let tmp = tempdir();
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);
        let yaml = "\
location:
  directory: tools
formats:
  - extensions: [\".py\"]
    parser: parser:rye/core/python/ast
    signature:
      prefix: \"#\"
      after_shebang: true
metadata:
  rules:
    name:
      from: filename
      required: true
      match: filename
    category:
      from: path
      key: category
      required: true
      match: path
    version:
      from: path
      key: version
";
        sign_and_write_schema(&tmp, "tool", yaml, &sk);

        let reg = KindRegistry::load_base(&[tmp], &ts).unwrap();
        let tool = reg.get("tool").unwrap();

        let name_rule = tool.extraction_rules.get("name").unwrap();
        assert_eq!(name_rule.extractor, ExtractionRule::Filename);
        assert!(name_rule.required);
        assert_eq!(name_rule.match_kind, Some(MatchKind::Filename));

        let cat_rule = tool.extraction_rules.get("category").unwrap();
        assert_eq!(
            cat_rule.extractor,
            ExtractionRule::Path { key: "category".into() }
        );
        assert!(cat_rule.required);
        assert_eq!(cat_rule.match_kind, Some(MatchKind::Path));

        // Defaults: no required, no match_kind
        let ver_rule = tool.extraction_rules.get("version").unwrap();
        assert!(!ver_rule.required);
        assert_eq!(ver_rule.match_kind, None);
    }

    #[test]
    fn schema_rejects_unknown_match_value() {
        let tmp = tempdir();
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);
        let yaml = "\
location:
  directory: tools
formats:
  - extensions: [\".py\"]
    parser: parser:rye/core/python/ast
    signature:
      prefix: \"#\"
metadata:
  rules:
    name:
      from: filename
      match: bogus
";
        sign_and_write_schema(&tmp, "tool", yaml, &sk);

        let err = KindRegistry::load_base(&[tmp], &ts).unwrap_err();
        assert!(
            matches!(err, EngineError::SchemaLoaderError { ref reason }
                if reason.contains("unknown value") && reason.contains("bogus")),
            "expected match-value error, got: {err:?}"
        );
    }

    #[test]
    fn schema_rejects_unknown_rule_key() {
        let tmp = tempdir();
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);
        let yaml = "\
location:
  directory: tools
formats:
  - extensions: [\".py\"]
    parser: parser:rye/core/python/ast
    signature:
      prefix: \"#\"
metadata:
  rules:
    name:
      from: filename
      bogus: true
";
        sign_and_write_schema(&tmp, "tool", yaml, &sk);

        let err = KindRegistry::load_base(&[tmp], &ts).unwrap_err();
        assert!(
            matches!(err, EngineError::SchemaLoaderError { ref reason }
                if reason.contains("unknown key") && reason.contains("bogus")),
            "expected unknown-key error, got: {err:?}"
        );
    }

    #[test]
    fn schema_rejects_match_on_path_string_seq() {
        let tmp = tempdir();
        let sk = test_signing_key();
        let ts = test_trust_store(&sk);
        let yaml = "\
location:
  directory: tools
formats:
  - extensions: [\".py\"]
    parser: parser:rye/core/python/ast
    signature:
      prefix: \"#\"
metadata:
  rules:
    things:
      from: path_string_seq
      key: things
      match: path
";
        sign_and_write_schema(&tmp, "tool", yaml, &sk);

        let err = KindRegistry::load_base(&[tmp], &ts).unwrap_err();
        assert!(
            matches!(err, EngineError::SchemaLoaderError { ref reason }
                if reason.contains("path_string_seq") && reason.contains("scalar")),
            "expected scalar-anchor error, got: {err:?}"
        );
    }

    // ── validate_metadata_anchoring direct tests ───────────────────

    fn anchor_rule(extractor: ExtractionRule, required: bool, match_kind: Option<MatchKind>) -> MetadataRule {
        MetadataRule { extractor, required, match_kind }
    }

    #[test]
    fn anchoring_happy_path_root_category() {
        // File at the root of `.ai/directives/` — category MUST be ""
        // and `required: true` accepts empty-string as a value.
        let ai_root = Path::new("/proj/.ai");
        let file = Path::new("/proj/.ai/directives/hello.md");
        let parsed = serde_json::json!({"category": ""});
        let mut rules = HashMap::new();
        rules.insert(
            "name".to_string(),
            anchor_rule(ExtractionRule::Filename, true, Some(MatchKind::Filename)),
        );
        rules.insert(
            "category".to_string(),
            anchor_rule(
                ExtractionRule::Path { key: "category".into() },
                true,
                Some(MatchKind::Path),
            ),
        );
        validate_metadata_anchoring(&parsed, &rules, "directives", ai_root, file)
            .expect("root-category empty string should pass");
    }

    #[test]
    fn anchoring_happy_path_with_category() {
        let ai_root = Path::new("/proj/.ai");
        let file = Path::new("/proj/.ai/tools/rye/core/sign.py");
        let parsed = serde_json::json!({"category": "rye/core"});
        let mut rules = HashMap::new();
        rules.insert(
            "name".to_string(),
            anchor_rule(ExtractionRule::Filename, true, Some(MatchKind::Filename)),
        );
        rules.insert(
            "category".to_string(),
            anchor_rule(
                ExtractionRule::Path { key: "category".into() },
                true,
                Some(MatchKind::Path),
            ),
        );
        validate_metadata_anchoring(&parsed, &rules, "tools", ai_root, file)
            .expect("nested category should pass");
    }

    #[test]
    fn anchoring_rejects_filename_mismatch() {
        let ai_root = Path::new("/proj/.ai");
        let file = Path::new("/proj/.ai/directives/hello.md");
        // metadata says "goodbye" but filename is "hello"
        let parsed = serde_json::json!({"name": "goodbye"});
        let mut rules = HashMap::new();
        rules.insert(
            "name".to_string(),
            anchor_rule(
                ExtractionRule::Path { key: "name".into() },
                true,
                Some(MatchKind::Filename),
            ),
        );
        let err = validate_metadata_anchoring(&parsed, &rules, "directives", ai_root, file)
            .unwrap_err();
        assert!(
            matches!(err, MetadataAnchoringError::FilenameMismatch {
                ref filename,
                ref metadata_value,
                ..
            } if filename == "hello" && metadata_value == "goodbye"),
            "expected FilenameMismatch, got: {err:?}"
        );
    }

    #[test]
    fn anchoring_rejects_path_mismatch() {
        let ai_root = Path::new("/proj/.ai");
        let file = Path::new("/proj/.ai/tools/rye/core/sign.py");
        // metadata says "rye/wrong" but path is "rye/core"
        let parsed = serde_json::json!({"category": "rye/wrong"});
        let mut rules = HashMap::new();
        rules.insert(
            "category".to_string(),
            anchor_rule(
                ExtractionRule::Path { key: "category".into() },
                true,
                Some(MatchKind::Path),
            ),
        );
        let err =
            validate_metadata_anchoring(&parsed, &rules, "tools", ai_root, file).unwrap_err();
        assert!(
            matches!(err, MetadataAnchoringError::PathMismatch {
                ref path_value,
                ref metadata_value,
                ..
            } if path_value == "rye/core" && metadata_value == "rye/wrong"),
            "expected PathMismatch, got: {err:?}"
        );
    }

    #[test]
    fn anchoring_rejects_missing_required() {
        let ai_root = Path::new("/proj/.ai");
        let file = Path::new("/proj/.ai/directives/hello.md");
        // metadata.name absent
        let parsed = serde_json::json!({});
        let mut rules = HashMap::new();
        rules.insert(
            "name".to_string(),
            anchor_rule(
                ExtractionRule::Path { key: "name".into() },
                true,
                Some(MatchKind::Filename),
            ),
        );
        let err = validate_metadata_anchoring(&parsed, &rules, "directives", ai_root, file)
            .unwrap_err();
        assert!(
            matches!(err, MetadataAnchoringError::MissingRequired { ref field, .. }
                if field == "name"),
            "expected MissingRequired, got: {err:?}"
        );
    }

    #[test]
    fn anchoring_rejects_outside_kind_directory() {
        let ai_root = Path::new("/proj/.ai");
        // File is NOT under .ai/tools
        let file = Path::new("/proj/.ai/directives/hello.md");
        let parsed = serde_json::json!({"category": "anything"});
        let mut rules = HashMap::new();
        rules.insert(
            "category".to_string(),
            anchor_rule(
                ExtractionRule::Path { key: "category".into() },
                true,
                Some(MatchKind::Path),
            ),
        );
        let err =
            validate_metadata_anchoring(&parsed, &rules, "tools", ai_root, file).unwrap_err();
        assert!(
            matches!(err, MetadataAnchoringError::OutsideKindDirectory { .. }),
            "expected OutsideKindDirectory, got: {err:?}"
        );
    }

    #[test]
    fn anchoring_no_required_no_match_passes() {
        let ai_root = Path::new("/proj/.ai");
        let file = Path::new("/proj/.ai/directives/hello.md");
        let parsed = serde_json::json!({});
        let mut rules = HashMap::new();
        rules.insert(
            "version".to_string(),
            anchor_rule(ExtractionRule::Path { key: "version".into() }, false, None),
        );
        validate_metadata_anchoring(&parsed, &rules, "directives", ai_root, file)
            .expect("optional field absent is fine");
    }
}
