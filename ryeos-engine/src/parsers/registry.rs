//! Registry of parser descriptors discovered on disk.
//!
//! Mirrors `KindRegistry::load_base` in shape and intent: walks the
//! parser tree under each search root's `.ai/<parser-kind-directory>/`
//! (typically `.ai/parsers/`), strict-deserializes every YAML file as
//! a `ParserDescriptor`, verifies the signature using the envelope
//! declared by the `parser` kind schema, and stores them by canonical
//! ref like `parser:rye/core/yaml/yaml`. Parser kind identity is
//! implicit from location (parsers are their own kind) — there is no
//! discriminator field on the descriptor.
//!
//! The raw signed-YAML loader is **necessary** here — going through the
//! normal kind resolution path would require a parser registry that
//! does not yet exist (cycle).
//!
//! Precedence semantics:
//!   * **base layer** (`load_base`) — refs MUST be unique across all
//!     base roots. A duplicate is recorded as a `DuplicateRef` and the
//!     boot validator MUST treat it as fatal. The data path retains the
//!     first occurrence purely so the in-memory registry stays
//!     well-formed until the boot validator fails the boot.
//!   * **project overlay** (`with_project_overlay`) — the ONLY
//!     sanctioned override path. Descriptors found here REPLACE any
//!     base entry with the same canonical ref.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::contracts::SignatureEnvelope;
use crate::error::EngineError;
use crate::kind_registry::KindRegistry;
use crate::trust::TrustStore;
use crate::AI_DIR;

use super::descriptor::ParserDescriptor;

/// A canonical-ref collision detected during multi-root base loading.
///
/// The base layer's contract is **uniqueness** — the project overlay
/// is the only sanctioned override path. `ParserRegistry::load_base`
/// records every duplicate here as a structured boot issue rather
/// than silently shadowing one of the descriptors. The data path
/// keeps the first occupant only so the in-memory registry remains
/// well-formed; callers MUST fail boot whenever this list is
/// non-empty (the boot validator emits `BootIssue::DuplicateParserRef`
/// for each entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateRef {
    pub canonical_ref: String,
    /// `paths[0]` is the descriptor that won the slot;
    /// `paths[1..]` are the shadowed duplicates.
    pub paths: Vec<PathBuf>,
}

/// In-memory parser tool descriptor table.
///
/// Lookup is by canonical ref (`parser:rye/core/yaml/yaml`).
/// Fingerprint is over the verified signed bytes of every descriptor
/// the registry contains (plus the base fingerprint for overlay
/// composition).
#[derive(Debug, Clone)]
pub struct ParserRegistry {
    descriptors: HashMap<String, ParserDescriptor>,
    fingerprint: String,
}

impl ParserRegistry {
    pub fn empty() -> Self {
        Self {
            descriptors: HashMap::new(),
            fingerprint: "empty".to_owned(),
        }
    }

    /// Construct from in-memory entries — used by tests and harnesses
    /// that don't want to write signed YAML to disk.
    pub fn from_entries<I>(entries: I) -> Self
    where
        I: IntoIterator<Item = (String, ParserDescriptor)>,
    {
        let descriptors: HashMap<String, ParserDescriptor> = entries.into_iter().collect();
        let mut keys: Vec<&String> = descriptors.keys().collect();
        keys.sort();
        let mut bytes: Vec<u8> = Vec::new();
        for k in keys {
            bytes.extend_from_slice(k.as_bytes());
            if let Ok(s) = serde_json::to_vec(&descriptors[k]) {
                bytes.extend_from_slice(&s);
            }
        }
        Self {
            descriptors,
            fingerprint: lillux::cas::sha256_hex(&bytes),
        }
    }

    /// Load the base registry by walking the user + system parser roots.
    ///
    /// `roots` are paths to **bundle / space roots** (parents of the
    /// `.ai/` dir), matching the convention `KindRegistry::load_base`
    /// uses for kind schema search roots — except here we descend into
    /// `.ai/parsers/**` (or whatever `directory` the `parser` kind
    /// schema declares) rather than `.ai/config/engine/kinds/**`.
    ///
    /// **Contract: base canonical refs MUST be unique.** Returns the
    /// loaded registry and a `Vec<DuplicateRef>` listing every
    /// canonical ref encountered in more than one root. The data path
    /// keeps the first occurrence only so the registry stays
    /// well-formed; callers MUST fail boot when the duplicate list is
    /// non-empty (the boot validator emits
    /// `BootIssue::DuplicateParserRef`). Project-level overrides go
    /// through `with_project_overlay`, which is the only sanctioned
    /// override path.
    pub fn load_base(
        roots: &[PathBuf],
        trust_store: &TrustStore,
        kinds: &KindRegistry,
    ) -> Result<(Self, Vec<DuplicateRef>), EngineError> {
        let bootstrap = ParserBootstrap::derive(kinds)?;

        let mut descriptors: HashMap<String, ParserDescriptor> = HashMap::new();
        let mut origin_paths: HashMap<String, PathBuf> = HashMap::new();
        let mut duplicates: HashMap<String, Vec<PathBuf>> = HashMap::new();
        let mut fingerprint_data: Vec<u8> = Vec::new();

        for root in roots {
            let tools_root = root.join(AI_DIR).join(&bootstrap.directory);
            if !tools_root.exists() {
                // Be permissive — a bundle that does not declare any
                // tool is legitimate.
                continue;
            }
            walk_tools(
                &tools_root,
                &tools_root,
                &mut descriptors,
                &mut origin_paths,
                &mut duplicates,
                &mut fingerprint_data,
                false,
                trust_store,
                &bootstrap,
            )?;
        }

        let fingerprint = lillux::cas::sha256_hex(&fingerprint_data);

        let mut dup_list: Vec<DuplicateRef> = duplicates
            .into_iter()
            .map(|(canonical_ref, paths)| DuplicateRef { canonical_ref, paths })
            .collect();
        dup_list.sort_by(|a, b| a.canonical_ref.cmp(&b.canonical_ref));

        Ok((
            Self {
                descriptors,
                fingerprint,
            },
            dup_list,
        ))
    }

    /// Apply a project overlay: descriptors found in the project's
    /// `.ai/<parser-kind-directory>/` REPLACE any base entry with the
    /// same canonical ref.
    ///
    /// **This is the ONLY sanctioned override path.** All other
    /// discovery layers (`load_base` across user + system roots) are
    /// required to be unique — a duplicate ref across base roots is a
    /// fatal boot issue, not a silent shadow. Per-project customization
    /// flows exclusively through this overlay.
    pub fn with_project_overlay(
        &self,
        project_root: &Path,
        trust_store: &TrustStore,
        kinds: &KindRegistry,
    ) -> Result<Self, EngineError> {
        let bootstrap = ParserBootstrap::derive(kinds)?;
        let tools_root = project_root.join(AI_DIR).join(&bootstrap.directory);
        if !tools_root.exists() {
            return Ok(self.clone());
        }

        let mut descriptors = self.descriptors.clone();
        let mut fingerprint_data = self.fingerprint.as_bytes().to_vec();
        // `origin_paths` doubles as the within-overlay seen-set:
        // every descriptor added during this walk records its path
        // here. `walk_tools` (with `replace_existing=true`) checks
        // this map to fail loud when the same canonical ref appears
        // twice WITHIN the overlay (a project authoring bug —
        // there's no good reason to ship two parser files for the
        // same ref). Base-vs-overlay collisions stay allowed: the
        // overlay starts with `descriptors` pre-populated by the
        // base, but `origin_paths` starts empty so an overlay file
        // that overrides a base entry is still permitted.
        let mut origin_paths: HashMap<String, PathBuf> = HashMap::new();
        let mut duplicates: HashMap<String, Vec<PathBuf>> = HashMap::new();

        walk_tools(
            &tools_root,
            &tools_root,
            &mut descriptors,
            &mut origin_paths,
            &mut duplicates,
            &mut fingerprint_data,
            true,
            trust_store,
            &bootstrap,
        )?;

        let fingerprint = lillux::cas::sha256_hex(&fingerprint_data);
        Ok(Self {
            descriptors,
            fingerprint,
        })
    }

    pub fn get(&self, parser_ref: &str) -> Option<&ParserDescriptor> {
        self.descriptors.get(parser_ref)
    }

    pub fn contains(&self, parser_ref: &str) -> bool {
        self.descriptors.contains_key(parser_ref)
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn refs(&self) -> impl Iterator<Item = &str> {
        self.descriptors.keys().map(|s| s.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &ParserDescriptor)> {
        self.descriptors
            .iter()
            .map(|(k, v)| (k.as_str(), v))
    }

    pub fn len(&self) -> usize {
        self.descriptors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.descriptors.is_empty()
    }
}

impl Default for ParserRegistry {
    fn default() -> Self {
        Self::empty()
    }
}

/// Bootstrap facts derived from the `parser` kind schema — directory
/// to scan, accepted extensions (sans leading dot), and the signature
/// envelope to verify with. Computed once per `load_base` call so the
/// loader's hardcoded `"parsers"` / `"yaml"|"yml"` / `"#"` tuple is
/// gone — the kind schema is now load-bearing.
#[derive(Debug)]
struct ParserBootstrap {
    /// Directory under `.ai/` to walk (e.g. `"parsers"`).
    directory: String,
    /// Accepted file extensions WITHOUT the leading dot
    /// (e.g. `{"yaml", "yml"}`).
    extensions: HashSet<String>,
    /// Signature envelope every descriptor file in this tree uses.
    /// All extensions in the parser kind must agree on this envelope.
    signature: SignatureEnvelope,
}

impl ParserBootstrap {
    fn derive(kinds: &KindRegistry) -> Result<Self, EngineError> {
        let parser_kind = kinds.get("parser").ok_or_else(|| EngineError::SchemaLoaderError {
            reason:
                "parser kind schema not registered — required for parser bootstrap"
                    .into(),
        })?;

        if parser_kind.extensions.is_empty() {
            return Err(EngineError::SchemaLoaderError {
                reason: "parser kind schema declares no extensions".into(),
            });
        }

        let signature = parser_kind.extensions[0].signature.clone();
        for spec in parser_kind.extensions.iter().skip(1) {
            if spec.signature != signature {
                return Err(EngineError::SchemaLoaderError {
                    reason: format!(
                        "parser kind schema extensions disagree on signature \
                         envelope: `{}` vs `{}`",
                        parser_kind.extensions[0].ext, spec.ext
                    ),
                });
            }
        }

        // Parser descriptors are YAML data files, never shebang-bearing
        // scripts. The `after_shebang` envelope flag is meaningless
        // here and would silently shift signature verification to the
        // wrong line. Reject it loudly so authors fix the schema rather
        // than chase a confusing signature mismatch later.
        if signature.after_shebang {
            return Err(EngineError::SchemaLoaderError {
                reason: "parser kind schema declares after_shebang=true on \
                         signature envelope, but parser descriptors are not \
                         shebang-bearing scripts; remove after_shebang from \
                         the parser kind schema"
                    .into(),
            });
        }

        // Reject compound extensions (e.g. `.schema.yaml`, `.tar.gz`).
        // The walker matches via `Path::extension()` which only returns
        // the LAST component, so a compound extension would silently
        // match every file ending in the trailing component. Aggregate
        // ALL offenders so the author sees them in one shot.
        let bad_exts: Vec<&str> = parser_kind
            .extensions
            .iter()
            .filter(|s| s.ext.trim_start_matches('.').contains('.'))
            .map(|s| s.ext.as_str())
            .collect();
        if !bad_exts.is_empty() {
            return Err(EngineError::SchemaLoaderError {
                reason: format!(
                    "parser kind schema declares compound extension(s) \
                     [{}]; parser bootstrap matches via the final extension \
                     component only, so compound suffixes would silently \
                     over-match. Use single-component extensions like `.yaml`",
                    bad_exts.join(", ")
                ),
            });
        }

        let extensions = parser_kind
            .extensions
            .iter()
            .map(|s| s.ext.trim_start_matches('.').to_owned())
            .collect();

        Ok(Self {
            directory: parser_kind.directory.clone(),
            extensions,
            signature,
        })
    }
}

/// Recursive descent into the parser kind's declared directory under
/// `.ai/`. Every file matching the parser kind's accepted extensions
/// is strict-deserialized + signature-verified using the parser kind's
/// declared signature envelope.
fn walk_tools(
    tools_root: &Path,
    cur: &Path,
    descriptors: &mut HashMap<String, ParserDescriptor>,
    origin_paths: &mut HashMap<String, PathBuf>,
    duplicates: &mut HashMap<String, Vec<PathBuf>>,
    fingerprint_data: &mut Vec<u8>,
    replace_existing: bool,
    trust_store: &TrustStore,
    bootstrap: &ParserBootstrap,
) -> Result<(), EngineError> {
    let entries = match std::fs::read_dir(cur) {
        Ok(d) => d,
        Err(e) => {
            return Err(EngineError::SchemaLoaderError {
                reason: format!("cannot read parsers dir {}: {e}", cur.display()),
            });
        }
    };

    let mut sorted: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    sorted.sort();

    for path in sorted {
        if path.is_dir() {
            walk_tools(
                tools_root,
                &path,
                descriptors,
                origin_paths,
                duplicates,
                fingerprint_data,
                replace_existing,
                trust_store,
                bootstrap,
            )?;
            continue;
        }

        let ext_match = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|e| bootstrap.extensions.contains(e))
            .unwrap_or(false);
        if !ext_match {
            continue;
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                return Err(EngineError::SchemaLoaderError {
                    reason: format!("cannot read {}: {e}", path.display()),
                });
            }
        };

        // Verify signature using the parser kind's declared envelope.
        verify_signature_with_envelope(&path, &content, trust_store, &bootstrap.signature)?;

        let stripped = lillux::signature::strip_signature_lines_with_envelope(
            &content,
            &bootstrap.signature.prefix,
            bootstrap.signature.suffix.as_deref(),
        );
        let descriptor: ParserDescriptor = serde_yaml::from_str(&stripped).map_err(|e| {
            EngineError::SchemaLoaderError {
                reason: format!(
                    "{}: invalid parser descriptor: {e}",
                    path.display()
                ),
            }
        })?;

        if descriptor.parser_api_version != 1 {
            return Err(EngineError::SchemaLoaderError {
                reason: format!(
                    "{}: parser_api_version must be 1 (got {})",
                    path.display(),
                    descriptor.parser_api_version
                ),
            });
        }

        let canonical_ref = derive_canonical_ref(tools_root, &path).map_err(|reason| {
            EngineError::SchemaLoaderError { reason }
        })?;

        if replace_existing {
            // Within a single overlay walk, a canonical ref MUST
            // appear at most once. The overlay is the only sanctioned
            // override path; two overlay files mapping to the same
            // ref would silently become last-write-wins by traversal
            // order, which is a project authoring bug — fail loud.
            // (Base-vs-overlay collisions are still allowed: the
            // descriptors map may already hold a base entry for this
            // ref, but `origin_paths` starts empty for the overlay
            // walk, so we only fire on intra-overlay duplicates.)
            if let Some(prior) = origin_paths.get(&canonical_ref) {
                return Err(EngineError::SchemaLoaderError {
                    reason: format!(
                        "duplicate parser canonical ref `{canonical_ref}` \
                         within project overlay: {} and {} both map to it; \
                         project overlays must declare each parser exactly once",
                        prior.display(),
                        path.display()
                    ),
                });
            }
            descriptors.insert(canonical_ref.clone(), descriptor);
            origin_paths.insert(canonical_ref, path.clone());
            fingerprint_data.extend_from_slice(content.as_bytes());
        } else {
            use std::collections::hash_map::Entry;
            match descriptors.entry(canonical_ref.clone()) {
                Entry::Vacant(e) => {
                    e.insert(descriptor);
                    origin_paths.insert(canonical_ref, path.clone());
                    fingerprint_data.extend_from_slice(content.as_bytes());
                }
                Entry::Occupied(_) => {
                    // Base layer must be unique. We retain the first
                    // occupant only so the in-memory registry stays
                    // well-formed; the collision is recorded so the
                    // boot validator can fail boot loudly.
                    let entry = duplicates.entry(canonical_ref.clone()).or_insert_with(|| {
                        // Seed with the original winner so the issue
                        // report points at both files.
                        let winner = origin_paths
                            .get(&canonical_ref)
                            .cloned()
                            .unwrap_or_else(|| PathBuf::from("<unknown>"));
                        vec![winner]
                    });
                    entry.push(path.clone());
                }
            }
        }
    }

    Ok(())
}

/// Verify a parser descriptor signature using the supplied envelope.
/// Identical machinery as `kind_registry::load_and_verify_kind_schema`,
/// except the envelope is derived from the parser kind schema rather
/// than hardcoded.
fn verify_signature_with_envelope(
    path: &Path,
    content: &str,
    trust_store: &TrustStore,
    envelope: &SignatureEnvelope,
) -> Result<(), EngineError> {
    let prefix = envelope.prefix.as_str();
    let suffix = envelope.suffix.as_deref();

    let header = lillux::signature::parse_signature_line(
        content.lines().next().unwrap_or(""),
        prefix,
        suffix,
    );

    let canonical_ref = path.display().to_string();

    match header {
        Some(h) => {
            let body = lillux::signature::strip_signature_lines_with_envelope(
                content, prefix, suffix,
            );
            let actual = lillux::signature::content_hash(&body);
            if actual != h.content_hash {
                return Err(EngineError::ContentHashMismatch {
                    canonical_ref,
                    expected: h.content_hash,
                    actual,
                });
            }
            let signer = trust_store
                .get(&h.signer_fingerprint)
                .ok_or_else(|| EngineError::UntrustedSigner {
                    canonical_ref: canonical_ref.clone(),
                    fingerprint: h.signer_fingerprint.clone(),
                })?;
            if !lillux::signature::verify_signature(
                &h.content_hash,
                &h.signature_b64,
                &signer.verifying_key,
            ) {
                return Err(EngineError::SignatureVerificationFailed {
                    canonical_ref,
                    reason: "Ed25519 signature verification failed".into(),
                });
            }
            Ok(())
        }
        None => Err(EngineError::SignatureMissing { canonical_ref }),
    }
}

/// Derive `tool:<rel-path-without-ext>` from the file's path under
/// `tools/`.
fn derive_canonical_ref(tools_root: &Path, path: &Path) -> Result<String, String> {
    let rel = path
        .strip_prefix(tools_root)
        .map_err(|e| format!("strip {}: {e}", path.display()))?;
    let mut parts: Vec<String> = rel
        .with_extension("")
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(|s| s.to_string()))
        .collect();
    if parts.is_empty() {
        return Err(format!("empty relative path for {}", path.display()));
    }
    let last = parts.pop().unwrap();
    if !parts.is_empty() {
        Ok(format!("parser:{}/{last}", parts.join("/")))
    } else {
        Ok(format!("parser:{last}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::{TrustStore, TrustedSigner};
    use lillux::crypto::SigningKey;
    use std::fs;

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rye_parser_reg_{}_{}",
            std::process::id(),
            rand_u64()
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

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn trust_store(sk: &SigningKey) -> TrustStore {
        let vk = sk.verifying_key();
        TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: crate::trust::compute_fingerprint(&vk),
            verifying_key: vk,
            label: None,
        }])
    }

    fn write_signed(path: &Path, content: &str, sk: &SigningKey) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        // Parser descriptor YAMLs now require `output_schema`.
        // Inject an empty mapping for descriptors that don't exercise
        // contract semantics. Detected by path containing `parsers`.
        let owned;
        let body: &str =
            if path.to_string_lossy().contains("parsers") && !content.contains("output_schema") {
                owned = format!(
                    "{content}output_schema:\n  root_type: mapping\n  required: {{}}\n"
                );
                &owned
            } else {
                content
            };
        let signed = lillux::signature::sign_content(body, sk, "#", None);
        fs::write(path, signed).unwrap();
    }

    /// The default parser kind schema used by most tests — directory
    /// `parsers`, accepts `.yaml`/`.yml`, signature envelope `#`.
    /// Mirrors the live bundle at
    /// `ryeos-bundles/core/.ai/config/engine/kinds/parser/`.
    const DEFAULT_PARSER_KIND_SCHEMA: &str = "\
location:
  directory: parsers
formats:
  - extensions: [\".yaml\", \".yml\"]
    parser: parser:rye/core/yaml/yaml
    signature:
      prefix: \"#\"
";

    /// Build a `KindRegistry` containing a parser kind schema. Tests
    /// pass this into `ParserRegistry::load_base` so the loader can
    /// derive its directory / extensions / signature envelope without
    /// hardcoding.
    fn parser_kinds(yaml: &str, sk: &SigningKey, ts: &TrustStore) -> KindRegistry {
        let kinds_dir = tempdir();
        let parser_dir = kinds_dir.join("parser");
        fs::create_dir_all(&parser_dir).unwrap();
        let yaml_owned = if yaml.contains("composed_value_contract") {
            yaml.to_string()
        } else {
            { let with_contract = format!("{yaml}composed_value_contract:\n  root_type: mapping\n  required: {{}}\n"); if with_contract.contains("composer:") { with_contract } else { format!("{with_contract}composer: rye/core/identity\n") } }
        };
        let signed = lillux::signature::sign_content(&yaml_owned, sk, "#", None);
        fs::write(parser_dir.join("parser.kind-schema.yaml"), signed).unwrap();
        KindRegistry::load_base(&[kinds_dir], ts)
            .expect("test parser kind schema loads")
    }

    fn default_parser_kinds(sk: &SigningKey, ts: &TrustStore) -> KindRegistry {
        parser_kinds(DEFAULT_PARSER_KIND_SCHEMA, sk, ts)
    }

    #[test]
    fn loads_parser_descriptor() {
        let root = tempdir();
        let sk = signing_key();
        let ts = trust_store(&sk);

        let yaml = "\
version: \"1.0.0\"
description: \"yaml document parser\"
executor_id: \"native:parser_yaml_document\"
parser_api_version: 1
parser_config:
  require_mapping: true
";
        let p = root
            .join(".ai/parsers/rye/core/yaml/yaml.yaml");
        write_signed(&p, yaml, &sk);

        let (reg, dups) = ParserRegistry::load_base(&[root], &ts, &default_parser_kinds(&sk, &ts)).unwrap();
        assert!(dups.is_empty());
        let d = reg.get("parser:rye/core/yaml/yaml").unwrap();
        assert_eq!(d.executor_id, "native:parser_yaml_document");
        assert_eq!(d.parser_api_version, 1);
        assert!(!reg.fingerprint().is_empty());
    }

    /// Anything in `.ai/parsers/` MUST be a parser descriptor. The
    /// previous "silently skip non-parser YAML" behavior relied on a
    /// discriminator field that no longer exists — the parser kind is
    /// implicit from location, so an unrecognized YAML here is a hard
    /// load error (fail loud).
    #[test]
    fn rejects_non_parser_yaml_in_parsers_dir() {
        let root = tempdir();
        let sk = signing_key();
        let ts = trust_store(&sk);

        let yaml = "\
version: \"1.0.0\"
executor_id: \"native:bash\"
";
        let p = root.join(".ai/parsers/rye/core/something.yaml");
        write_signed(&p, yaml, &sk);

        let err = ParserRegistry::load_base(&[root], &ts, &default_parser_kinds(&sk, &ts)).unwrap_err();
        assert!(
            matches!(err, EngineError::SchemaLoaderError { .. }),
            "expected SchemaLoaderError, got: {err:?}"
        );
    }

    #[test]
    fn rejects_unsigned_descriptor() {
        let root = tempdir();
        let sk = signing_key();
        let ts = trust_store(&sk);

        let yaml = "\
version: \"1.0.0\"
executor_id: \"native:parser_yaml_document\"
parser_api_version: 1
parser_config: {}
";
        let p = root.join(".ai/parsers/rye/core/x/x.yaml");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, yaml).unwrap();

        let err = ParserRegistry::load_base(&[root], &ts, &default_parser_kinds(&sk, &ts)).unwrap_err();
        assert!(matches!(err, EngineError::SignatureMissing { .. }));
    }

    /// Two roots define the same canonical ref → loader returns Ok
    /// with a `DuplicateRef` and the boot validator fails boot.
    ///
    /// Pins the base-layer uniqueness contract: the project overlay
    /// is the ONLY sanctioned override path, so a duplicate across
    /// base roots is always a hard issue (never silently shadowed).
    /// The data path retains the first occupant only so the in-memory
    /// registry stays well-formed until boot fails.
    #[test]
    fn duplicate_canonical_ref_in_base_is_reported() {
        let root_a = tempdir();
        let root_b = tempdir();
        let sk = signing_key();
        let ts = trust_store(&sk);

        let yaml_a = "\
version: \"1.0.0\"
description: \"A wins\"
executor_id: \"native:parser_yaml_document\"
parser_api_version: 1
parser_config: {}
";
        let yaml_b = "\
version: \"2.0.0\"
description: \"B loses\"
executor_id: \"native:parser_yaml_document\"
parser_api_version: 1
parser_config: {}
";
        let p_a = root_a.join(".ai/parsers/rye/core/yaml/yaml.yaml");
        let p_b = root_b.join(".ai/parsers/rye/core/yaml/yaml.yaml");
        write_signed(&p_a, yaml_a, &sk);
        write_signed(&p_b, yaml_b, &sk);

        let (reg, dups) =
            ParserRegistry::load_base(&[root_a.clone(), root_b.clone()], &ts, &default_parser_kinds(&sk, &ts)).unwrap();
        let d = reg.get("parser:rye/core/yaml/yaml").unwrap();
        assert_eq!(d.version, "1.0.0");
        assert_eq!(d.description.as_deref(), Some("A wins"));
        // single canonical ref retained
        assert_eq!(reg.len(), 1);

        // …but the loader MUST surface the collision so the boot
        // validator can fail loud instead of silently dropping the
        // shadowed descriptor.
        assert_eq!(dups.len(), 1);
        assert_eq!(dups[0].canonical_ref, "parser:rye/core/yaml/yaml");
        assert_eq!(dups[0].paths.len(), 2);
        assert!(dups[0].paths[0].starts_with(&root_a));
        assert!(dups[0].paths[1].starts_with(&root_b));
    }

    /// Project overlay (replace_existing=true) DOES win over base —
    /// asserts the asymmetry between cross-base first-found-wins and
    /// project-overlay last-write-wins. Documents intent.
    #[test]
    fn project_overlay_replaces_base_entry() {
        let base = tempdir();
        let project = tempdir();
        let sk = signing_key();
        let ts = trust_store(&sk);

        let yaml_base = "\
version: \"1.0.0\"
description: \"base\"
executor_id: \"native:parser_yaml_document\"
parser_api_version: 1
parser_config: {}
";
        let yaml_proj = "\
version: \"9.0.0\"
description: \"project override\"
executor_id: \"native:parser_yaml_document\"
parser_api_version: 1
parser_config: {}
";
        write_signed(
            &base.join(".ai/parsers/rye/core/yaml/yaml.yaml"),
            yaml_base,
            &sk,
        );
        write_signed(
            &project.join(".ai/parsers/rye/core/yaml/yaml.yaml"),
            yaml_proj,
            &sk,
        );

        let (base_reg, _dups) = ParserRegistry::load_base(&[base], &ts, &default_parser_kinds(&sk, &ts)).unwrap();
        let overlaid = base_reg.with_project_overlay(&project, &ts, &default_parser_kinds(&sk, &ts)).unwrap();
        let d = overlaid.get("parser:rye/core/yaml/yaml").unwrap();
        assert_eq!(d.version, "9.0.0");
        assert_eq!(d.description.as_deref(), Some("project override"));
    }

    /// The parser kind schema is load-bearing: if it declares a
    /// non-default `directory` field, the loader MUST scan that
    /// directory rather than the historical hardcoded `parsers`.
    #[test]
    fn parser_kind_schema_drives_directory_name() {
        let root = tempdir();
        let sk = signing_key();
        let ts = trust_store(&sk);

        let kinds = parser_kinds(
            "\
location:
  directory: custom_parsers
formats:
  - extensions: [\".yaml\"]
    parser: parser:rye/core/yaml/yaml
    signature:
      prefix: \"#\"
",
            &sk,
            &ts,
        );

        let yaml = "\
version: \"1.0.0\"
executor_id: \"native:parser_yaml_document\"
parser_api_version: 1
parser_config: {}
";
        // Write to the schema-declared directory; the legacy
        // hardcoded `parsers` dir does NOT exist.
        let p = root.join(".ai/custom_parsers/rye/core/yaml/yaml.yaml");
        write_signed(&p, yaml, &sk);

        let (reg, _dups) = ParserRegistry::load_base(&[root], &ts, &kinds).unwrap();
        assert!(
            reg.get("parser:rye/core/yaml/yaml").is_some(),
            "loader must walk schema-declared `custom_parsers/` directory; \
             refs = {:?}",
            reg.refs().collect::<Vec<_>>()
        );
    }

    /// Without a `parser` kind in the registry, the parser loader has
    /// no directory / extensions / envelope to scan with — surface a
    /// loud SchemaLoaderError instead of silently scanning nothing.
    #[test]
    fn missing_parser_kind_is_hard_error() {
        let root = tempdir();
        let sk = signing_key();
        let ts = trust_store(&sk);

        // KindRegistry with NO parser kind registered.
        let empty_kinds = KindRegistry::empty();

        let err = ParserRegistry::load_base(&[root], &ts, &empty_kinds).unwrap_err();
        let msg = format!("{err}");
        assert!(
            matches!(err, EngineError::SchemaLoaderError { .. }),
            "expected SchemaLoaderError, got: {err:?}"
        );
        assert!(
            msg.contains("parser kind schema"),
            "error must mention the missing parser kind schema; got: {msg}"
        );
    }

    /// Within a single project overlay walk, two files mapping to
    /// the same canonical ref MUST fail loud. The overlay is the
    /// only sanctioned override path; if two overlay files mapped to
    /// the same ref the winner would silently depend on directory
    /// traversal order, which is a project authoring bug. (Base-vs-
    /// overlay collisions are still allowed — see
    /// `project_overlay_replaces_base_entry`.)
    #[test]
    fn duplicate_canonical_ref_within_overlay_is_hard_error() {
        let base = tempdir();
        let project = tempdir();
        let sk = signing_key();
        let ts = trust_store(&sk);

        // Empty base — the duplicate is purely intra-overlay.
        let (base_reg, _) =
            ParserRegistry::load_base(&[base], &ts, &default_parser_kinds(&sk, &ts)).unwrap();

        let yaml_a = "\
version: \"1.0.0\"
description: \"overlay file A\"
executor_id: \"native:parser_yaml_document\"
parser_api_version: 1
parser_config: {}
";
        let yaml_b = "\
version: \"2.0.0\"
description: \"overlay file B\"
executor_id: \"native:parser_yaml_document\"
parser_api_version: 1
parser_config: {}
";
        // Both `.yaml` and `.yml` strip to canonical ref
        // `parser:rye/core/yaml/yaml` — the parser kind accepts both
        // extensions, and `derive_canonical_ref` strips the suffix.
        // This is exactly the kind of project authoring bug the
        // duplicate check is meant to catch.
        write_signed(
            &project.join(".ai/parsers/rye/core/yaml/yaml.yaml"),
            yaml_a,
            &sk,
        );
        write_signed(
            &project.join(".ai/parsers/rye/core/yaml/yaml.yml"),
            yaml_b,
            &sk,
        );

        let err = base_reg
            .with_project_overlay(&project, &ts, &default_parser_kinds(&sk, &ts))
            .expect_err("intra-overlay duplicate must be a hard error");
        let msg = format!("{err}");
        assert!(
            matches!(err, EngineError::SchemaLoaderError { .. }),
            "expected SchemaLoaderError for intra-overlay duplicate, got: {err:?}"
        );
        assert!(
            msg.contains("duplicate parser canonical ref")
                && msg.contains("parser:rye/core/yaml/yaml"),
            "error must name the duplicated ref so the project author \
             can fix the offending files; got: {msg}"
        );
    }

    /// Parser kind schema declaring a compound extension (e.g.
    /// `.schema.yaml`) MUST be rejected at bootstrap, because the
    /// walker matches via `Path::extension()` which only returns the
    /// LAST component — a compound suffix would silently over-match
    /// every file ending in the trailing component.
    #[test]
    fn rejects_compound_extension_in_parser_kind() {
        let sk = signing_key();
        let ts = trust_store(&sk);

        let kinds = parser_kinds(
            "\
location:
  directory: parsers
formats:
  - extensions: [\".schema.yaml\", \".tar.gz\"]
    parser: parser:rye/core/yaml/yaml
    signature:
      prefix: \"#\"
",
            &sk,
            &ts,
        );

        let err = ParserBootstrap::derive(&kinds)
            .expect_err("compound extensions must be rejected at bootstrap");
        let msg = format!("{err}");
        assert!(
            matches!(err, EngineError::SchemaLoaderError { .. }),
            "expected SchemaLoaderError, got: {err:?}"
        );
        assert!(
            msg.contains("compound extension")
                && msg.contains(".schema.yaml")
                && msg.contains(".tar.gz"),
            "error must aggregate every offending compound extension; \
             got: {msg}"
        );
    }

    /// Parser descriptors are YAML data files, not shebang-bearing
    /// scripts. A parser kind schema declaring `after_shebang: true`
    /// on its signature envelope would silently shift signature
    /// verification to the wrong line — reject it loudly at bootstrap.
    #[test]
    fn rejects_after_shebang_on_parser_kind_envelope() {
        let sk = signing_key();
        let ts = trust_store(&sk);

        let kinds = parser_kinds(
            "\
location:
  directory: parsers
formats:
  - extensions: [\".yaml\"]
    parser: parser:rye/core/yaml/yaml
    signature:
      prefix: \"#\"
      after_shebang: true
",
            &sk,
            &ts,
        );

        let err = ParserBootstrap::derive(&kinds)
            .expect_err("after_shebang on parser kind must be rejected");
        let msg = format!("{err}");
        assert!(
            matches!(err, EngineError::SchemaLoaderError { .. }),
            "expected SchemaLoaderError, got: {err:?}"
        );
        assert!(
            msg.contains("after_shebang"),
            "error must name the offending after_shebang flag; got: {msg}"
        );
    }

    /// `ParserDescriptor` uses `deny_unknown_fields`; an unknown
    /// top-level key in a parser YAML must fail the strict deserialize
    /// in the loader.
    #[test]
    fn descriptor_deny_unknown_fields() {
        let root = tempdir();
        let sk = signing_key();
        let ts = trust_store(&sk);

        let yaml = "\
version: \"1.0.0\"
executor_id: \"native:parser_yaml_document\"
parser_api_version: 1
parser_config: {}
totally_made_up_field: hi
";
        let p = root.join(".ai/parsers/rye/core/y/y.yaml");
        write_signed(&p, yaml, &sk);

        let err = ParserRegistry::load_base(&[root], &ts, &default_parser_kinds(&sk, &ts)).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("unknown field") || msg.contains("totally_made_up_field"),
            "expected unknown-field rejection, got: {msg}"
        );
    }
}
